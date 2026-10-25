use std::collections::HashMap;

use zpdf_core::{Error, ParseLimits, PdfDict, PdfObject, Result};

mod footprint;
pub use footprint::{box_downscale, box_downscale_rgba, box_filter_size};

pub type ImageId = u32;

/// Display-list image store. Entries remain stable because render commands keep
/// only an [`ImageId`]; callers bound memory by the store/document lifetime.
#[derive(Debug)]
pub struct ImageCache {
    images: HashMap<ImageId, DecodedImage>,
    next_id: ImageId,
    bytes_used: u64,
}

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub has_alpha: bool,
    /// True only for PDF `/ImageMask` (including inline `/IM true`) stencils.
    /// This is deliberately distinct from `has_alpha`: ordinary transparent
    /// images must keep their source RGB when rendered inside Type3 glyphs.
    pub is_image_mask: bool,
    pub premultiplied: bool,
}

impl ImageCache {
    pub fn new() -> Self {
        Self {
            images: HashMap::new(),
            next_id: 0,
            bytes_used: 0,
        }
    }

    pub fn get(&self, id: ImageId) -> Option<&DecodedImage> {
        self.images.get(&id)
    }

    /// Remove and return an image from the cache.
    ///
    /// This is intended for consumers such as document converters that need to
    /// take ownership of decoded pixels after they have finished inspecting the
    /// display list. Any display-list command that still refers to `id` becomes
    /// unusable after this call.
    pub fn remove(&mut self, id: ImageId) -> Option<DecodedImage> {
        let image = self.images.remove(&id)?;
        self.bytes_used = self
            .bytes_used
            .saturating_sub(u64::try_from(image.data.capacity()).unwrap_or(u64::MAX));
        Some(image)
    }

    pub fn insert(&mut self, image: DecodedImage) -> ImageId {
        self.try_insert_with_limit(image, u64::MAX)
            .expect("image id space exhausted")
    }

    /// Admit an image without evicting any existing ID. Returns `None` when
    /// the aggregate decoded-byte limit would be exceeded or no ID remains.
    pub fn try_insert_with_limit(
        &mut self,
        image: DecodedImage,
        max_bytes: u64,
    ) -> Option<ImageId> {
        // Charge the retained allocation, not merely its initialized length:
        // decoded buffers commonly reserve their final RGBA size up front.
        let image_bytes = u64::try_from(image.data.capacity()).ok()?;
        let new_total = self.bytes_used.checked_add(image_bytes)?;
        if new_total > max_bytes {
            return None;
        }
        let id = self.next_free_id()?;
        self.images.insert(id, image);
        self.bytes_used = new_total;
        Some(id)
    }

    fn next_free_id(&mut self) -> Option<ImageId> {
        let start = self.next_id;
        loop {
            let candidate = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if !self.images.contains_key(&candidate) {
                return Some(candidate);
            }
            if self.next_id == start {
                return None;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Total decoded image bytes currently retained by this store.
    pub fn bytes_used(&self) -> u64 {
        self.bytes_used
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
    CalGray(zpdf_color::CalGrayParams),
    CalRGB(zpdf_color::CalRGBParams),
    /// CIE L*a*b* with the parameters from its PDF colour-space dictionary.
    Lab {
        white_point: [f64; 3],
        range: [f64; 4],
    },
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
    /// Separation (`n == 1`) or DeviceN/NChannel (`n` colorants). Samples are
    /// colorant tints (0..=1 after `/Decode`) mapped through the tint
    /// transform into `alternate`. Without a usable transform a tint renders
    /// as `gray(1 − max tint)`, which keeps colorant polarity (0 = no ink).
    Tint {
        n: u8,
        transform: Option<std::sync::Arc<zpdf_color::PdfFunction>>,
        alternate: Box<ResolvedColorSpace>,
    },
}

/// Most colorants a DeviceN space may have (ISO 32000-1 Annex C), and so the
/// most components any image sample can carry.
const MAX_COMPONENTS: usize = 32;

/// Manual impl because transforms have no structural equality: two `Icc`
/// spaces are equal when they share the same compiled transform (used by the
/// DCT component sniffer and tests, not as a cache key).
impl PartialEq for ResolvedColorSpace {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CalGray(a), Self::CalGray(b)) => a == b,
            (Self::CalRGB(a), Self::CalRGB(b)) => a == b,
            (Self::Gray, Self::Gray) | (Self::Rgb, Self::Rgb) | (Self::Cmyk, Self::Cmyk) => true,
            (
                Self::Lab {
                    white_point: w1,
                    range: r1,
                },
                Self::Lab {
                    white_point: w2,
                    range: r2,
                },
            ) => w1 == w2 && r1 == r2,
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
            (
                Self::Tint {
                    n: n1,
                    transform: t1,
                    alternate: a1,
                },
                Self::Tint {
                    n: n2,
                    transform: t2,
                    alternate: a2,
                },
            ) => {
                n1 == n2
                    && a1 == a2
                    && match (t1, t2) {
                        (None, None) => true,
                        (Some(t1), Some(t2)) => std::sync::Arc::ptr_eq(t1, t2),
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

impl ResolvedColorSpace {
    /// Components per pixel in the *image stream* (Indexed samples are single
    /// palette indices regardless of the base space).
    fn components(&self) -> usize {
        match self {
            Self::Gray | Self::CalGray(_) | Self::Indexed { .. } => 1,
            Self::Rgb | Self::CalRGB(_) | Self::Lab { .. } => 3,
            Self::Cmyk => 4,
            // The compiled transform is authoritative. Trusting the public
            // `ncomp` hint here could make malformed caller input index past
            // the fixed four-component scratch arrays below.
            Self::Icc { transform, .. } => transform.components(),
            Self::Tint { n, .. } => *n as usize,
        }
    }

    fn validate(&self, depth: u8) -> Result<()> {
        if depth > 8 {
            return Err(Error::StreamDecode(
                "image colour space nesting exceeds the limit".into(),
            ));
        }
        match self {
            Self::Icc { ncomp, transform } => {
                let actual = transform.components();
                if !matches!(actual, 1 | 3 | 4) || *ncomp as usize != actual {
                    return Err(Error::StreamDecode(format!(
                        "ICCBased component hint {ncomp} does not match transform ({actual})"
                    )));
                }
            }
            Self::Indexed { base, .. } => {
                if matches!(base.as_ref(), Self::Indexed { .. }) {
                    return Err(Error::StreamDecode(
                        "Indexed colour space cannot have an Indexed base".into(),
                    ));
                }
                base.validate(depth + 1)?;
            }
            Self::Tint { n, alternate, .. } => {
                if *n == 0 || *n as usize > MAX_COMPONENTS {
                    return Err(Error::StreamDecode(format!(
                        "Separation/DeviceN colour space with {n} colorants"
                    )));
                }
                if matches!(alternate.as_ref(), Self::Indexed { .. } | Self::Tint { .. }) {
                    return Err(Error::StreamDecode(
                        "Separation/DeviceN alternate cannot be a special colour space".into(),
                    ));
                }
                alternate.validate(depth + 1)?;
            }
            Self::Lab { white_point, range }
                if white_point.iter().any(|v| !v.is_finite() || *v <= 0.0)
                    || range.iter().any(|v| !v.is_finite())
                    || range[0] > range[1]
                    || range[2] > range[3] =>
            {
                return Err(Error::StreamDecode(
                    "invalid Lab colour-space parameters".into(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

/// Map a device/CIE colour-space *name* to its resolved form. Shared with the
/// caller-side resolution helper so the name table lives in one place.
pub fn colorspace_from_name(name: &str) -> Option<ResolvedColorSpace> {
    match name {
        "DeviceGray" | "G" => Some(ResolvedColorSpace::Gray),
        "DeviceRGB" | "RGB" => Some(ResolvedColorSpace::Rgb),
        "CalGray" => Some(ResolvedColorSpace::CalGray(
            zpdf_color::CalGrayParams::from_pdf_dict(&PdfDict::new(), &mut |_| None),
        )),
        "CalRGB" => Some(ResolvedColorSpace::CalRGB(
            zpdf_color::CalRGBParams::from_pdf_dict(&PdfDict::new(), &mut |_| None),
        )),
        "DeviceCMYK" | "CMYK" => Some(ResolvedColorSpace::Cmyk),
        _ => None,
    }
}

pub fn decode_image_xobject(decoded_data: &[u8], dict: &PdfDict) -> Result<DecodedImage> {
    decode_image_xobject_with_limits(decoded_data, dict, &ParseLimits::default())
}

/// Decode an image XObject using explicit security limits.
pub fn decode_image_xobject_with_limits(
    decoded_data: &[u8],
    dict: &PdfDict,
    limits: &ParseLimits,
) -> Result<DecodedImage> {
    decode_image_xobject_resolved_with_limits(decoded_data, dict, [0, 0, 0], None, limits)
}

/// Decode an image XObject, painting `/ImageMask true` stencils in `fill_rgb`
/// (the current graphics-state fill colour). Non-mask images ignore `fill_rgb`.
pub fn decode_image_xobject_with_fill(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
) -> Result<DecodedImage> {
    decode_image_xobject_with_fill_and_limits(decoded_data, dict, fill_rgb, &ParseLimits::default())
}

/// Decode an image XObject with a stencil fill and explicit security limits.
pub fn decode_image_xobject_with_fill_and_limits(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
    limits: &ParseLimits,
) -> Result<DecodedImage> {
    decode_image_xobject_resolved_with_limits(decoded_data, dict, fill_rgb, None, limits)
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
    decode_image_xobject_resolved_with_limits(
        decoded_data,
        dict,
        fill_rgb,
        colorspace,
        &ParseLimits::default(),
    )
}

/// Decode an image XObject with a pre-resolved colour space and explicit
/// security limits.
pub fn decode_image_xobject_resolved_with_limits(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
    colorspace: Option<ResolvedColorSpace>,
    limits: &ParseLimits,
) -> Result<DecodedImage> {
    // JPEG 2000 first: the codestream carries its own dimensions, colour
    // space, bit depth and alpha, and /ColorSpace + /BitsPerComponent may
    // legally be absent from the dict (spec 7.4.9). A present /ColorSpace
    // overrides the codestream's colour declaration, so infer it here rather
    // than after this branch. A stencil mask is never JPX-coded (its samples
    // are 1-bit and may happen to start with a JPEG 2000 signature), so it
    // keeps its own path below.
    if !is_image_mask(dict) && is_jpx_encoded(dict, decoded_data) {
        let cs = colorspace.or_else(|| {
            dict.get("ColorSpace")
                .is_some()
                .then(|| infer_colorspace(dict))
        });
        if let Some(cs) = &cs {
            cs.validate(0)?;
        }
        return decode_jpx_image(decoded_data, dict, cs.as_ref(), limits.max_image_pixels);
    }

    let width_value = dict.get_i64("Width")?;
    let height_value = dict.get_i64("Height")?;
    let width = u32::try_from(width_value)
        .ok()
        .filter(|&v| v != 0)
        .ok_or_else(|| Error::StreamDecode(format!("invalid image Width {width_value}")))?;
    let height = u32::try_from(height_value)
        .ok()
        .filter(|&v| v != 0)
        .ok_or_else(|| Error::StreamDecode(format!("invalid image Height {height_value}")))?;
    let pixel_count = (width as u64).saturating_mul(height as u64);

    // Stencil mask: 1 bpc, paints `fill_rgb` where the sample selects the page
    // (default /Decode [0 1] → sample 0 paints; [1 0] inverts).
    if is_image_mask(dict) {
        if pixel_count > limits.max_image_pixels {
            return Err(Error::StreamDecode(format!(
                "image {width}x{height} exceeds the {}-pixel limit",
                limits.max_image_pixels
            )));
        }
        let invert = mask_decode_inverts(dict);
        return decode_image_mask(decoded_data, width, height, fill_rgb, invert);
    }

    let cs = colorspace.unwrap_or_else(|| infer_colorspace(dict));
    cs.validate(0)?;

    if is_dct_encoded(dict) {
        // The JPEG's own geometry (recovered from the decoded sample length)
        // is authoritative here, so do not reject a stale/large dict /Width ×
        // /Height before decode_dct_image has a chance to correct it.
        return decode_dct_image(
            decoded_data,
            width,
            height,
            &cs,
            dict,
            limits.max_image_pixels,
        );
    }

    if pixel_count > limits.max_image_pixels {
        return Err(Error::StreamDecode(format!(
            "image {width}x{height} exceeds the {}-pixel limit",
            limits.max_image_pixels
        )));
    }

    let bpc = bits_per_component(dict)?;

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

/// Fold a decoded `/SMask` (luminosity soft mask) into `image` as alpha. The
/// mask must come through the same full image-decode path as any image (so
/// filters, predictors, sub-byte bpc and /Decode all apply); the decoded gray
/// level is the alpha. A mask whose dimensions differ from the image is
/// bilinearly resampled to the image size. RGB is premultiplied as the alpha
/// folds in, because both backends (tiny-skia `PixmapRef::from_bytes` and the
/// wgpu `Rgba8Unorm` upload) treat the bytes as premultiplied RGBA.
pub fn apply_smask_image(image: &mut DecodedImage, mask: &DecodedImage) {
    if (mask.width, mask.height) == (image.width, image.height)
        && image.width != 0
        && image.height != 0
    {
        for (px, mask_px) in image
            .data
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(mask.data.as_chunks::<4>().0)
        {
            multiply_pixel_alpha(px, mask_px[0]);
        }
        image.has_alpha = true;
        image.premultiplied = true;
        return;
    }
    // The mask is DeviceGray, so its decoded R channel is the gray level.
    let alpha: Vec<u8> = mask
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[0])
        .collect();
    fold_alpha_plane(image, &alpha, mask.width, mask.height);
}

/// The `/Matte` array of a soft-mask stream dictionary: the colour (in the
/// parent image's colour space) its samples were pre-blended with.
pub fn smask_matte(smask_dict: &PdfDict) -> Option<Vec<f64>> {
    let values = smask_dict.get("Matte")?.as_array().ok()?;
    if values.is_empty() || values.len() > MAX_COMPONENTS {
        return None;
    }
    values
        .iter()
        .map(|value| value.as_f64().ok().filter(|v| v.is_finite()))
        .collect()
}

/// [`apply_smask_image`] for a soft mask carrying `/Matte` (ISO 32000-1
/// §11.6.5.3): the parent's colours were pre-blended with the matte colour
/// `m`, so each is un-premultiplied — `c = m + (c′ − m) / α`, clamped; pixels
/// with `α = 0` are left alone — before the alpha folds in.
///
/// `matte` holds the parent colour space's components and `colorspace` is
/// that space (as passed to the image decode); without it, 1/3/4 components
/// are read as DeviceGray/RGB/CMYK. The un-premultiply runs on the decoded
/// RGB, which is exact for Gray/RGB parents (the overwhelmingly common case,
/// with a black or white matte) and an approximation for non-linear spaces.
/// `matte: None` (or an unconvertible one) behaves as [`apply_smask_image`].
pub fn apply_smask_image_with_matte(
    image: &mut DecodedImage,
    mask: &DecodedImage,
    matte: Option<&[f64]>,
    colorspace: Option<&ResolvedColorSpace>,
) {
    let Some(matte) = matte.and_then(|matte| matte_to_rgb(matte, colorspace)) else {
        apply_smask_image(image, mask);
        return;
    };
    if image.width == 0 || image.height == 0 || mask.width == 0 || mask.height == 0 {
        return;
    }
    let alpha: Vec<u8> = mask
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[0])
        .collect();
    let resampled;
    let plane: &[u8] = if (mask.width, mask.height) == (image.width, image.height) {
        &alpha
    } else {
        resampled = resample_bilinear(&alpha, mask.width, mask.height, image.width, image.height);
        &resampled
    };
    for (px, &a) in image.data.as_chunks_mut::<4>().0.iter_mut().zip(plane) {
        // Only opaque, not-yet-premultiplied pixels carry pre-blended colour.
        if a != 0 && a != 255 && px[3] == 255 {
            for (c, &m) in px[..3].iter_mut().zip(&matte) {
                let unblended = m as f32 + (*c as f32 - m as f32) * 255.0 / a as f32;
                *c = unblended.round().clamp(0.0, 255.0) as u8;
            }
        }
        multiply_pixel_alpha(px, a);
    }
    image.has_alpha = true;
    image.premultiplied = true;
}

/// Convert a `/Matte` colour to 8-bit RGB through the parent colour space.
fn matte_to_rgb(matte: &[f64], colorspace: Option<&ResolvedColorSpace>) -> Option<[u8; 3]> {
    let inferred;
    let cs = match colorspace {
        Some(cs) => cs,
        None => {
            inferred = match matte.len() {
                1 => ResolvedColorSpace::Gray,
                3 => ResolvedColorSpace::Rgb,
                4 => ResolvedColorSpace::Cmyk,
                _ => return None,
            };
            &inferred
        }
    };
    if matte.len() != cs.components() || cs.validate(0).is_err() {
        return None;
    }
    Some(match cs {
        // An Indexed parent's matte is a palette index.
        ResolvedColorSpace::Indexed {
            base,
            hival,
            lookup,
        } => {
            let index = matte[0].round().clamp(0.0, *hival as f64) as u8;
            let palette = build_indexed_rgb_palette(base, *hival, lookup);
            palette[index as usize]
        }
        ResolvedColorSpace::Tint {
            transform,
            alternate,
            ..
        } => tint_to_rgb(transform.as_deref(), alternate, matte),
        other => float_components_to_rgb(other, matte),
    })
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
    apply_stencil_mask_with_limits(
        image,
        mask_data,
        mask_width,
        mask_height,
        invert,
        &ParseLimits::default(),
    );
}

/// Fold a `/Mask` stencil stream into an image using explicit security limits.
pub fn apply_stencil_mask_with_limits(
    image: &mut DecodedImage,
    mask_data: &[u8],
    mask_width: u32,
    mask_height: u32,
    invert: bool,
    limits: &ParseLimits,
) {
    if (mask_width as u64).saturating_mul(mask_height as u64) > limits.max_image_pixels {
        tracing::warn!(
            "/Mask stencil {mask_width}x{mask_height} exceeds the {}-pixel limit, ignored",
            limits.max_image_pixels
        );
        return;
    }
    let row_bytes = (mask_width as usize).div_ceil(8);
    if (mask_width, mask_height) == (image.width, image.height)
        && mask_width != 0
        && mask_height != 0
    {
        for (index, px) in image.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let row = index / mask_width as usize;
            if row >= mask_height as usize {
                break;
            }
            let col = index % mask_width as usize;
            let byte = mask_data
                .get(row * row_bytes + col / 8)
                .copied()
                .unwrap_or(0xFF);
            let sample = (byte >> (7 - (col % 8))) & 1;
            let masked = (sample == 1) != invert;
            multiply_pixel_alpha(px, if masked { 0 } else { 255 });
        }
        image.has_alpha = true;
        image.premultiplied = true;
        return;
    }
    let Some(alpha_len) = (mask_width as usize).checked_mul(mask_height as usize) else {
        tracing::warn!("/Mask stencil dimensions exceed addressable memory, ignored");
        return;
    };
    let mut alpha = Vec::with_capacity(alpha_len);
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
        Some(PdfObject::Array(a)) => a.first().and_then(|o| o.as_f64().ok()) == Some(1.0),
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
    // L2 Fix: Use checked arithmetic to prevent overflow in capacity calculation
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("image mask pixel count overflow".into()))?;
    let rgba_size = pixel_count
        .checked_mul(4)
        .ok_or_else(|| Error::StreamDecode("image mask RGBA buffer size overflow".into()))?;

    let row_bytes = (width as usize).div_ceil(8);
    let mut rgba = Vec::with_capacity(rgba_size);
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
        is_image_mask: true,
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
    if !matches!(head, "CalGray" | "CalRGB") {
        if let Some(cs) = colorspace_from_name(head) {
            return cs;
        }
    }
    match head {
        "CalGray" | "CalRGB" => {
            let empty = PdfDict::new();
            let params = arr.get(1).and_then(|o| o.as_dict().ok()).unwrap_or(&empty);
            if head == "CalGray" {
                ResolvedColorSpace::CalGray(zpdf_color::CalGrayParams::from_pdf_dict(
                    params,
                    &mut |_| None,
                ))
            } else {
                ResolvedColorSpace::CalRGB(zpdf_color::CalRGBParams::from_pdf_dict(
                    params,
                    &mut |_| None,
                ))
            }
        }
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
                PdfObject::Array(base) => Some(infer_colorspace_array(base)),
                _ => None,
            });
            let hival = arr.get(2).and_then(|o| o.as_f64().ok());
            let lookup = arr.get(3).and_then(|o| match o {
                PdfObject::String(s) => Some(s.as_bytes().to_vec()),
                _ => None,
            });
            match (base, hival, lookup) {
                (Some(base), Some(h), Some(lookup))
                    if (0.0..=255.0).contains(&h) && h.fract() == 0.0 =>
                {
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
        "Lab" => match arr.get(1) {
            Some(PdfObject::Dict(params)) => lab_colorspace(params),
            _ => {
                tracing::warn!("Lab image colour space has no parameter dictionary");
                ResolvedColorSpace::Gray
            }
        },
        "Separation" | "DeviceN" => infer_tint_colorspace(head == "Separation", arr),
        other => {
            tracing::warn!("unsupported colour space {other}, treating as gray");
            ResolvedColorSpace::Gray
        }
    }
}

/// The Lab space described by a `[/Lab params]` parameter dictionary, with
/// the D50 white point and `[-100 100 -100 100]` range defaults for missing
/// or malformed entries.
pub fn lab_colorspace(params: &PdfDict) -> ResolvedColorSpace {
    let white_point = numeric_array::<3>(params.get("WhitePoint"))
        .filter(|v| v.iter().all(|n| n.is_finite() && *n > 0.0))
        .unwrap_or([0.9642, 1.0, 0.8249]);
    let range = numeric_array::<4>(params.get("Range"))
        .filter(|v| v.iter().all(|n| n.is_finite()) && v[0] <= v[1] && v[2] <= v[3])
        .unwrap_or([-100.0, 100.0, -100.0, 100.0]);
    ResolvedColorSpace::Lab { white_point, range }
}

/// Inline `[/Separation name alt fn]` / `[/DeviceN names alt fn …]`. Only a
/// direct-dictionary tint transform (types 2/3) is reachable without object
/// access; otherwise the tint keeps colorant polarity via the gray fallback.
fn infer_tint_colorspace(is_separation: bool, arr: &[PdfObject]) -> ResolvedColorSpace {
    let n = if is_separation {
        1
    } else {
        match arr.get(1) {
            Some(PdfObject::Array(names)) if !names.is_empty() => names.len(),
            _ => {
                tracing::warn!("DeviceN image colour space has no colorant names");
                return ResolvedColorSpace::Gray;
            }
        }
    };
    let Ok(n) = u8::try_from(n) else {
        return ResolvedColorSpace::Gray;
    };
    let alternate = match arr.get(2) {
        Some(PdfObject::Name(name)) => colorspace_from_name(name.as_str()),
        Some(PdfObject::Array(alt)) => Some(infer_colorspace_array(alt)),
        _ => None,
    }
    .filter(|alt| {
        !matches!(
            alt,
            ResolvedColorSpace::Indexed { .. } | ResolvedColorSpace::Tint { .. }
        )
    })
    .unwrap_or(ResolvedColorSpace::Gray);
    let transform = arr.get(3).and_then(|f| {
        zpdf_color::PdfFunction::parse_object(f, &mut |_| None).map(std::sync::Arc::new)
    });
    ResolvedColorSpace::Tint {
        n,
        transform,
        alternate: Box::new(alternate),
    }
}

fn numeric_array<const N: usize>(obj: Option<&PdfObject>) -> Option<[f64; N]> {
    let values = obj?.as_array().ok()?;
    if values.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = value.as_f64().ok()?;
    }
    Some(out)
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
    // Sniff only when the dict declares no filter: after an explicit
    // non-JPX filter (FlateDecode, DCTDecode, …) the bytes are whatever that
    // filter produced and a coincidental signature must not reroute them.
    let declared = match dict.get("Filter") {
        Some(PdfObject::Name(_)) => true,
        Some(PdfObject::Array(arr)) => !arr.is_empty(),
        _ => false,
    };
    if declared {
        return filter_says;
    }
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
    max_image_pixels: u64,
) -> Result<DecodedImage> {
    use hayro_jpeg2000::{ColorSpace as JpxColorSpace, DecodeSettings, DecoderContext, Image};
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

    // L3 Fix: Validate dimensions immediately after parsing header, before decode.
    // JPX/JPEG2000 dimensions are embedded in the codestream (not just the dict),
    // so we must parse the header first. We validate as early as possible: after
    // Image::new (header parse) but before image.decode() (full decompression).
    // Re-check the pixel limit against the *codestream* header dims, which
    // are authoritative and may differ from the dict's /Width × /Height.
    let (width, height) = (image.width(), image.height());
    let pixel_count = (width as u64).saturating_mul(height as u64);
    if pixel_count == 0 || pixel_count > max_image_pixels {
        return Err(Error::StreamDecode(format!(
            "JPX codestream {width}x{height} exceeds the {max_image_pixels}-pixel limit"
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
    let mut codestream_icc: Option<&[u8]> = None;
    let ncolor = match image.color_space() {
        JpxColorSpace::Gray => 1usize,
        JpxColorSpace::RGB => 3,
        JpxColorSpace::CMYK => 4,
        JpxColorSpace::Icc {
            profile,
            num_channels,
        } => {
            let n = supported_ncolor(*num_channels)?;
            codestream_icc = Some(profile.as_slice());
            n
        }
        JpxColorSpace::Unknown { num_channels } => supported_ncolor(*num_channels)?,
    };
    let has_alpha = image.has_alpha();
    let channels = ncolor + usize::from(has_alpha);

    let samples = catch_unwind(AssertUnwindSafe(|| {
        // hayro-jpeg2000 0.4 split decoding into a reusable `DecoderContext`
        // (owned scratch buffers) plus `Image::decode(&mut ctx)`, which now
        // returns a `DecodedImage` borrow rather than an interleaved `Vec<u8>`.
        // `data_u8()` re-interleaves the components back into the same layout
        // 0.3.5 returned, so the downstream per-pixel chunking is unchanged.
        let mut ctx = DecoderContext::default();
        image.decode(&mut ctx).map(|decoded| decoded.data_u8())
    }))
    .map_err(|_| Error::StreamDecode("JPXDecode: decoder panicked decoding codestream".into()))?
    .map_err(|e| Error::StreamDecode(format!("JPXDecode: {e}")))?;
    let pixel_count = usize::try_from(pixel_count)
        .map_err(|_| Error::StreamDecode("JPX pixel count does not fit memory size".into()))?;
    let expected_samples = pixel_count
        .checked_mul(channels)
        .ok_or_else(|| Error::StreamDecode("JPX decoded sample count overflow".into()))?;
    if samples.len() < expected_samples {
        return Err(Error::StreamDecode(format!(
            "JPX decoded data too short: {} bytes for {width}x{height}x{channels}",
            samples.len()
        )));
    }

    let smask_in_data = match dict.get_i64("SMaskInData").unwrap_or(0) {
        value @ 0..=2 => value,
        other => {
            tracing::warn!("invalid /SMaskInData {other}; ignoring codestream alpha");
            0
        }
    };
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

    let rgba_size = pixel_count
        .checked_mul(4)
        .ok_or_else(|| Error::StreamDecode("JPX RGBA buffer size overflow".into()))?;
    let mut rgba = Vec::with_capacity(rgba_size);
    let mut color = effective_cs.map(|cs| PixelColor::new(cs, pixel_count));
    for px in samples.chunks_exact(channels).take(pixel_count) {
        let [mut r, mut g, mut b] = match &mut color {
            // Indexed lookups clamp the index to `hival`.
            Some(color) => color.convert(&px[..ncolor]),
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
        is_image_mask: false,
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
    max_image_pixels: u64,
) -> Result<DecodedImage> {
    // DCTDecode was already run by the parser's filter pipeline, which emits
    // the JPEG's own components: gray (1 byte/pixel), RGB from YCbCr (3) or
    // CMYK (4; Adobe YCCK is converted to CMYK there). CMYK stays CMYK so the
    // DeviceCMYK conversion — SWOP polynomial, ICC profile or output intent —
    // and a 4-component /Decode apply here exactly as for raw CMYK samples.
    // The component count (and, when the JPEG disagrees with the dict, its
    // real geometry) is recovered from the data length.
    let (width, height, ncomp) = dct_layout(decoded_data.len(), width, height, cs)?;
    if (width as u64).saturating_mul(height as u64) > max_image_pixels {
        return Err(Error::StreamDecode(format!(
            "DCT image {width}x{height} exceeds the {max_image_pixels}-pixel limit"
        )));
    }

    // Keep the resolved space when it describes what the JPEG produced (ICC,
    // Lab, a Separation over a gray JPEG, …); otherwise interpret the
    // components by count. (Indexed is not a valid DCT colour space.)
    let sniffed = if cs.components() == ncomp && !matches!(cs, ResolvedColorSpace::Indexed { .. }) {
        cs.clone()
    } else {
        match ncomp {
            4 => ResolvedColorSpace::Cmyk,
            3 => ResolvedColorSpace::Rgb,
            _ => ResolvedColorSpace::Gray,
        }
    };

    // /Decode and colour-key /Mask entries apply to the JPEG's output
    // components; only honour them when their length matches what the JPEG
    // actually produced. The mismatching case is logged inside the extractors.
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

/// Recover `(width, height, components)` of decoded DCT samples. The parser
/// hands over bare samples, so the length is all there is: first the dict's
/// dimensions at 1, 3 or 4 bytes/pixel; failing that the JPEG disagrees with
/// the dict (a common writer bug), and the dict width — or else its height —
/// is kept with the other axis taken from the data, preferring the colour
/// space's own component count. The image then renders at the JPEG's real
/// size, stretched over the same unit square, instead of being dropped.
fn dct_layout(
    len: usize,
    width: u32,
    height: u32,
    cs: &ResolvedColorSpace,
) -> Result<(u32, u32, usize)> {
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("DCT image pixel count overflow".into()))?;
    if len >= pixel_count && len.is_multiple_of(pixel_count) {
        let ncomp = len / pixel_count;
        if matches!(ncomp, 1 | 3 | 4) {
            return Ok((width, height, ncomp));
        }
    }
    let preferred = cs.components();
    let candidates = [preferred, 3, 1, 4]
        .into_iter()
        .enumerate()
        .filter(|&(index, n)| matches!(n, 1 | 3 | 4) && (index == 0 || n != preferred))
        .map(|(_, n)| n);
    for (fixed, fixed_is_width) in [(width, true), (height, false)] {
        for ncomp in candidates.clone() {
            let Some(line) = (fixed as usize).checked_mul(ncomp) else {
                continue;
            };
            if len < line || !len.is_multiple_of(line) {
                continue;
            }
            let Ok(other) = u32::try_from(len / line) else {
                continue;
            };
            let (w, h) = if fixed_is_width {
                (width, other)
            } else {
                (other, height)
            };
            tracing::warn!(
                "DCT image dict says {width}x{height} but the JPEG decoded to {w}x{h} \
                 ({ncomp} components); using the JPEG dimensions"
            );
            return Ok((w, h, ncomp));
        }
    }
    Err(Error::StreamDecode(format!(
        "DCT decoded data length {len} does not fit a {width}x{height} image"
    )))
}

/// `/BitsPerComponent` (default 8 when absent or not a number). An integral
/// real such as `8.0` is accepted like the integer it spells.
fn bits_per_component(dict: &PdfDict) -> Result<u8> {
    let invalid = |value: &dyn std::fmt::Display| {
        Error::StreamDecode(format!("invalid BitsPerComponent {value}"))
    };
    let bpc = match dict.get("BitsPerComponent") {
        Some(PdfObject::Integer(value)) => u8::try_from(*value).map_err(|_| invalid(value))?,
        Some(PdfObject::Real(value)) if value.fract() == 0.0 && (1.0..=16.0).contains(value) => {
            *value as u8
        }
        Some(PdfObject::Real(value)) => return Err(invalid(value)),
        _ => 8,
    };
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(invalid(&bpc));
    }
    Ok(bpc)
}

/// Check the post-filter sample length against the `required` bytes,
/// returning whether the data is short (missing samples read as 0). Truncated streams
/// are common (a writer that stopped early, a damaged Flate tail) and viewers
/// render what is there with the missing samples read as 0; less than one
/// full row is treated as no usable image data.
fn check_sample_data_len(len: usize, row_bytes: usize, required: usize) -> Result<bool> {
    if len >= required {
        return Ok(false);
    }
    if len < row_bytes {
        return Err(Error::StreamDecode(format!(
            "image data too short: {len} bytes, expected {required}"
        )));
    }
    tracing::warn!("image data has {len} of {required} bytes; padding missing samples with 0");
    Ok(true)
}

/// `/Decode` as `[min, max]` pairs per component, if present with the
/// expected length.
fn decode_array(dict: &PdfDict, ncomp: usize) -> Option<Vec<f32>> {
    let arr = match dict.get("Decode") {
        Some(PdfObject::Array(a)) => a,
        _ => return None,
    };
    let expected = ncomp.checked_mul(2)?;
    if arr.len() != expected {
        tracing::warn!(
            "/Decode has {} numbers, expected {} — ignoring",
            arr.len(),
            expected
        );
        return None;
    }
    arr.iter()
        .map(|o| {
            let value = o.as_f64().ok()?;
            let value = value as f32;
            value.is_finite().then_some(value)
        })
        .collect()
}

/// `/Mask [min₁ max₁ …]` colour-key ranges, compared against RAW sample values
/// (before /Decode is applied), per spec 8.9.6.4.
fn color_key_ranges(dict: &PdfDict, ncomp: usize) -> Option<Vec<(u16, u16)>> {
    // A soft mask overrides /Mask (ISO 32000-1 Table 89, /SMask entry).
    if matches!(
        dict.get("SMask"),
        Some(PdfObject::Ref(_) | PdfObject::Stream(_))
    ) {
        return None;
    }
    let arr = match dict.get("Mask") {
        Some(PdfObject::Array(a)) => a,
        _ => return None,
    };
    let expected = ncomp.checked_mul(2)?;
    if arr.len() != expected {
        tracing::warn!(
            "colour-key /Mask has {} numbers, expected {} — ignoring",
            arr.len(),
            expected
        );
        return None;
    }
    let vals: Vec<u16> = arr
        .iter()
        .map(|o| {
            let value = o.as_f64().ok()?;
            value
                .is_finite()
                .then_some(value.clamp(0.0, 65535.0) as u16)
        })
        .collect::<Option<_>>()?;
    Some(
        vals.as_chunks::<2>()
            .0
            .iter()
            .map(|p| (p[0].min(p[1]), p[0].max(p[1])))
            .collect(),
    )
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
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("raw samples: pixel count overflow".into()))?;

    // L5 Fix: Check row_bytes computation for overflow before allocation
    let row_bytes = (width as usize)
        .checked_mul(ncomp)
        .and_then(|v| v.checked_mul(bpc as usize))
        .ok_or_else(|| Error::StreamDecode("raw samples: row bytes overflow".into()))?
        .div_ceil(8);
    let required_data = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("raw samples: data size overflow".into()))?;
    // Truncated streams are not copied or padded: the generic unpacker reads
    // missing samples as 0. Only take the contiguous 8-bpc fast path when every
    // row is present; otherwise it would leave the tail transparent instead of
    // filling it with opaque black.
    let short = check_sample_data_len(data.len(), row_bytes, required_data)?;

    let rgba_size = pixel_count
        .checked_mul(4)
        .ok_or_else(|| Error::StreamDecode("raw samples: RGBA buffer size overflow".into()))?;

    let luts = build_component_luts(bpc, cs, decode);
    // Palettes, tint tables and caches are resolved once per image. In
    // particular this avoids invoking an ICC transform separately for every
    // pixel when an Indexed palette has an ICC base.
    let mut color = PixelColor::new(cs, pixel_count);
    let mut comps = [0u8; MAX_COMPONENTS];

    // Fast path: 8-bit samples and no colour key — which is the shape of every
    // image that reaches here, since the parser's filter pipeline hands over
    // bytes (zune-jpeg output, Flate rows, …). The general loop below re-derives
    // each byte offset through a bit reader, re-checks the colour key and
    // re-reserves the output buffer *per pixel*; none of that is needed when the
    // samples already are bytes. Values come from the same LUTs and
    // `PixelColor`, so the two paths agree byte for byte.
    if bpc == 8 && color_key.is_none() && row_bytes == width as usize * ncomp && !short {
        // 8-bpc rows carry no padding, so pixels are contiguous. The loop is
        // monomorphized per component count, with Gray/RGB converted inline.
        let mut rgba = vec![0u8; rgba_size];
        match (cs, ncomp) {
            (ResolvedColorSpace::Gray, _) => {
                fill_rgba_8bpc::<1>(data, &mut rgba, &luts, |c| [c[0]; 3])
            }
            (ResolvedColorSpace::Rgb, _) => {
                fill_rgba_8bpc::<3>(data, &mut rgba, &luts, |c| [c[0], c[1], c[2]])
            }
            (_, 1) => fill_rgba_8bpc::<1>(data, &mut rgba, &luts, |c| color.convert(c)),
            (_, 3) => fill_rgba_8bpc::<3>(data, &mut rgba, &luts, |c| color.convert(c)),
            (_, 4) => fill_rgba_8bpc::<4>(data, &mut rgba, &luts, |c| color.convert(c)),
            _ => {
                for (src, dst) in data.chunks_exact(ncomp).zip(rgba.as_chunks_mut::<4>().0) {
                    for c in 0..ncomp {
                        comps[c] = luts[c][src[c] as usize];
                    }
                    let [r, g, b] = color.convert(&comps[..ncomp]);
                    *dst = [r, g, b, 255];
                }
            }
        }
        return Ok(DecodedImage {
            width,
            height,
            data: rgba,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });
    }

    let layout = SampleLayout {
        width,
        height,
        row_bytes,
        bpc,
        ncomp,
    };
    // Bitonal and other sub-byte gray scans are common: keep their conversion
    // inline rather than behind the generic per-pixel dispatch.
    let (rgba, any_masked) = match cs {
        ResolvedColorSpace::Gray => {
            unpack_rgba(data, &layout, &luts, color_key, rgba_size, |c| [c[0]; 3])
        }
        _ => unpack_rgba(data, &layout, &luts, color_key, rgba_size, |c| {
            color.convert(c)
        }),
    };

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: any_masked,
        is_image_mask: false,
        // Colour-key holes are transparent black, which is valid premultiplied
        // RGBA — the backends treat the bytes as premultiplied.
        premultiplied: any_masked,
    })
}

/// Geometry of packed raw samples (rows are byte-aligned).
struct SampleLayout {
    width: u32,
    height: u32,
    row_bytes: usize,
    bpc: u8,
    ncomp: usize,
}

/// The general inner loop of [`decode_raw_samples`]: unpack each pixel's
/// samples through a bit reader, key out colour-key matches on the raw values,
/// map the rest through the LUTs and `convert`. Returns the RGBA buffer and
/// whether any pixel was keyed out.
fn unpack_rgba(
    data: &[u8],
    layout: &SampleLayout,
    luts: &[Vec<u8>],
    color_key: Option<&[(u16, u16)]>,
    rgba_size: usize,
    mut convert: impl FnMut(&[u8]) -> [u8; 3],
) -> (Vec<u8>, bool) {
    let SampleLayout {
        width,
        height,
        row_bytes,
        bpc,
        ncomp,
    } = *layout;
    let mut rgba = Vec::with_capacity(rgba_size);
    let mut any_masked = false;
    let mut raw = [0u16; MAX_COMPONENTS];
    let mut comps = [0u8; MAX_COMPONENTS];
    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let mut bit = 0usize;
        for _col in 0..width {
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
            for c in 0..ncomp {
                let idx = if bpc == 16 {
                    (raw[c] >> 8) as usize
                } else {
                    raw[c] as usize
                };
                comps[c] = luts[c][idx];
            }
            let [r, g, b] = convert(&comps[..ncomp]);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    (rgba, any_masked)
}

/// The 8-bpc, colour-key-free inner loop of [`decode_raw_samples`]: map each
/// pixel's `N` bytes through the per-component LUTs, convert, and write
/// opaque RGBA (as many pixels as both buffers hold).
#[inline]
fn fill_rgba_8bpc<const N: usize>(
    data: &[u8],
    rgba: &mut [u8],
    luts: &[Vec<u8>],
    mut convert: impl FnMut(&[u8; N]) -> [u8; 3],
) {
    let luts: &[Vec<u8>; N] = luts.try_into().expect("one LUT per component");
    for (src, dst) in data
        .as_chunks::<N>()
        .0
        .iter()
        .zip(rgba.as_chunks_mut::<4>().0)
    {
        let comps: [u8; N] = std::array::from_fn(|c| luts[c][src[c] as usize]);
        let [r, g, b] = convert(&comps);
        *dst = [r, g, b, 255];
    }
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
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("ICC image pixel count overflow".into()))?;
    let luts = build_component_luts(bpc, cs, decode);
    let row_bytes = (width as usize)
        .checked_mul(ncomp)
        .and_then(|v| v.checked_mul(bpc as usize))
        .ok_or_else(|| Error::StreamDecode("ICC image row size overflow".into()))?
        .div_ceil(8);
    let required_data = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("ICC image data size overflow".into()))?;
    // Short data needs no copy here: `read_sample` reads missing samples as 0.
    check_sample_data_len(data.len(), row_bytes, required_data)?;
    let rgba_size = pixel_count
        .checked_mul(4)
        .ok_or_else(|| Error::StreamDecode("ICC image RGBA size overflow".into()))?;

    // Transform bounded batches instead of retaining full component, mask and
    // RGB planes (up to another 7 bytes/pixel) alongside the final RGBA image.
    const CHUNK_PIXELS: usize = 16 * 1024;
    let chunk_pixels = pixel_count.min(CHUNK_PIXELS);
    let mut comps = Vec::with_capacity(chunk_pixels.saturating_mul(ncomp));
    let mut masked = Vec::with_capacity(chunk_pixels);
    let mut rgb = Vec::with_capacity(chunk_pixels.saturating_mul(3));
    let mut rgba = Vec::with_capacity(rgba_size);
    let mut any_masked = false;

    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let mut bit = 0usize;
        for _col in 0..width as usize {
            let mut raw = [0u16; 4];
            for r in raw.iter_mut().take(ncomp) {
                *r = read_sample(data, row_start, &mut bit, bpc);
            }
            let is_masked = color_key.is_some_and(|ranges| {
                ranges
                    .iter()
                    .zip(&raw)
                    .all(|(&(lo, hi), &v)| lo <= v && v <= hi)
            });
            masked.push(is_masked);
            any_masked |= is_masked;
            for c in 0..ncomp {
                let idx = if bpc == 16 {
                    (raw[c] >> 8) as usize
                } else {
                    raw[c] as usize
                };
                comps.push(luts[c][idx]);
            }
            if masked.len() == CHUNK_PIXELS {
                append_icc_chunk(transform, &mut comps, &mut masked, &mut rgb, &mut rgba)?;
            }
        }
    }
    append_icc_chunk(transform, &mut comps, &mut masked, &mut rgb, &mut rgba)?;

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: any_masked,
        is_image_mask: false,
        // Colour-key holes are transparent black, which is valid premultiplied
        // RGBA — the backends treat the bytes as premultiplied.
        premultiplied: any_masked,
    })
}

fn append_icc_chunk(
    transform: &zpdf_color::IccTransform,
    comps: &mut Vec<u8>,
    masked: &mut Vec<bool>,
    rgb: &mut Vec<u8>,
    rgba: &mut Vec<u8>,
) -> Result<()> {
    if masked.is_empty() {
        return Ok(());
    }
    let rgb_len = masked
        .len()
        .checked_mul(3)
        .ok_or_else(|| Error::StreamDecode("ICC chunk size overflow".into()))?;
    rgb.resize(rgb_len, 0);
    transform.slice_to_rgb(comps, rgb)?;
    for (px, &is_masked) in rgb.as_chunks::<3>().0.iter().zip(masked.iter()) {
        if is_masked {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    comps.clear();
    masked.clear();
    Ok(())
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
                ResolvedColorSpace::Lab { range, .. } => {
                    let default = match c {
                        0 => (0.0, 100.0),
                        1 => (range[0] as f32, range[1] as f32),
                        _ => (range[2] as f32, range[3] as f32),
                    };
                    decode.map_or(default, |d| (d[c * 2], d[c * 2 + 1]))
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
                        ResolvedColorSpace::Lab { range, .. } => {
                            let (min, max) = match c {
                                0 => (0.0, 100.0),
                                1 => (range[0] as f32, range[1] as f32),
                                _ => (range[2] as f32, range[3] as f32),
                            };
                            let normalized = if (max - min).abs() <= f32::EPSILON {
                                0.0
                            } else {
                                (v - min) / (max - min)
                            };
                            (normalized.clamp(0.0, 1.0) * 255.0).round() as u8
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

/// Direct-mapped cache slots (`2^PIXEL_CACHE_BITS`) for colour spaces
/// converted per distinct sample tuple.
const PIXEL_CACHE_BITS: u32 = 12;
const PIXEL_CACHE_SLOTS: usize = 1 << PIXEL_CACHE_BITS;

/// Exact tint-transform evaluations one image may spend on cache misses. A
/// calculator function can run thousands of steps per evaluation, so a large
/// DeviceN image of mostly distinct pixels must be bounded: past the budget
/// the remaining misses interpolate a grid of transform samples instead.
const MAX_TINT_EVALUATIONS: u32 = 1 << 15;
/// Samples in that grid: `256²`, so two-colorant images stay exact.
const TINT_GRID_POINTS: usize = 1 << 16;
/// Colorants the grid supports (each lookup blends `2^n` corners); beyond
/// that, past-budget pixels take the gray tint fallback.
const MAX_TINT_GRID_COLORANTS: usize = 6;

/// Per-image sample → RGB conversion, built once so the per-pixel work is a
/// table lookup wherever the space allows: Indexed palettes and
/// single-colorant tints become tables, while multi-colorant tints (a tint
/// transform evaluation each) and Lab (`powf`-heavy) memoize recent sample
/// tuples. Every variant yields exactly what [`components_to_rgb`] would,
/// except multi-colorant tints past [`MAX_TINT_EVALUATIONS`].
enum PixelColor<'a> {
    Direct(&'a ResolvedColorSpace),
    /// RGB by Indexed palette index or by 8-bit Separation tint.
    Table(Vec<[u8; 3]>),
    /// Up to four components, packed into the key with a presence bit (an
    /// all-zero slot is empty).
    CachedNarrow {
        cs: &'a ResolvedColorSpace,
        slots: Vec<(u64, [u8; 3])>,
        miss: Miss,
    },
    CachedWide {
        cs: &'a ResolvedColorSpace,
        slots: Vec<Option<([u8; MAX_COMPONENTS], [u8; 3])>>,
        miss: Miss,
    },
}

/// How a cached conversion handles misses.
enum Miss {
    /// Exact conversion; the cache is dropped when it rarely hits.
    Exact(CacheStats),
    /// Tint evaluation within the image's budget, then the grid.
    Tint(TintBudget),
}

impl Miss {
    #[inline]
    fn hit(&mut self) {
        if let Self::Exact(stats) = self {
            stats.record(true);
        }
    }

    /// Convert a missed tuple; true when the cache should be dropped.
    fn convert(&mut self, cs: &ResolvedColorSpace, comps: &[u8]) -> ([u8; 3], bool) {
        match self {
            Self::Exact(stats) => (components_to_rgb(cs, comps), stats.record(false)),
            Self::Tint(budget) => (budget.convert(cs, comps), false),
        }
    }
}

/// Hit accounting over an image's first pixels: a cache that rarely hits
/// (photographic content) only adds overhead, so it is dropped.
#[derive(Default)]
struct CacheStats {
    lookups: u32,
    hits: u32,
}

impl CacheStats {
    const PROBE_WINDOW: u32 = 1 << 14;

    /// Record one lookup; true when the probe window just closed with under
    /// a quarter of lookups hitting.
    #[inline]
    fn record(&mut self, hit: bool) -> bool {
        if self.lookups >= Self::PROBE_WINDOW {
            return false;
        }
        self.lookups += 1;
        self.hits += u32::from(hit);
        self.lookups == Self::PROBE_WINDOW && self.hits < Self::PROBE_WINDOW / 4
    }
}

#[derive(Default)]
struct TintBudget {
    evaluations: u32,
    /// Built on first need; `Some(None)` when the colorant count has no grid.
    grid: Option<Option<TintGrid>>,
}

impl TintBudget {
    fn convert(&mut self, cs: &ResolvedColorSpace, comps: &[u8]) -> [u8; 3] {
        let ResolvedColorSpace::Tint {
            n,
            transform,
            alternate,
        } = cs
        else {
            return components_to_rgb(cs, comps);
        };
        if self.evaluations < MAX_TINT_EVALUATIONS {
            self.evaluations += 1;
            return components_to_rgb(cs, comps);
        }
        let grid = self.grid.get_or_insert_with(|| {
            tracing::warn!(
                "tint transform evaluation budget exhausted; interpolating the rest of the image"
            );
            TintGrid::build(transform.as_deref(), alternate, *n as usize)
        });
        match grid {
            Some(grid) => grid.lookup(comps),
            // gray(1 − max tint), as `tint_to_rgb` falls back to.
            None => [255 - comps.iter().copied().max().unwrap_or(0); 3],
        }
    }
}

/// A tint transform sampled on a regular grid over the colorant cube (axis 0
/// varying fastest), looked up by multilinear interpolation.
struct TintGrid {
    n: usize,
    side: usize,
    rgb: Vec<[u8; 3]>,
}

impl TintGrid {
    fn build(
        transform: Option<&zpdf_color::PdfFunction>,
        alternate: &ResolvedColorSpace,
        n: usize,
    ) -> Option<Self> {
        if n == 0 || n > MAX_TINT_GRID_COLORANTS {
            return None;
        }
        let points = |side: usize| side.checked_pow(n as u32).unwrap_or(usize::MAX);
        let mut side = 2;
        while side < 256 && points(side + 1) <= TINT_GRID_POINTS {
            side += 1;
        }
        let mut tints = [0.0f64; MAX_TINT_GRID_COLORANTS];
        let rgb = (0..points(side))
            .map(|index| {
                let mut rest = index;
                for tint in tints.iter_mut().take(n) {
                    *tint = (rest % side) as f64 / (side - 1) as f64;
                    rest /= side;
                }
                tint_to_rgb(transform, alternate, &tints[..n])
            })
            .collect();
        Some(Self { n, side, rgb })
    }

    fn lookup(&self, comps: &[u8]) -> [u8; 3] {
        let scale = (self.side - 1) as f32 / 255.0;
        let mut base = 0;
        let mut stride = 1;
        let mut strides = [0usize; MAX_TINT_GRID_COLORANTS];
        let mut fracs = [0.0f32; MAX_TINT_GRID_COLORANTS];
        for axis in 0..self.n {
            let pos = comps.get(axis).copied().unwrap_or(0) as f32 * scale;
            let cell = (pos as usize).min(self.side - 2);
            fracs[axis] = pos - cell as f32;
            strides[axis] = stride;
            base += cell * stride;
            stride *= self.side;
        }
        let mut acc = [0.0f32; 3];
        for corner in 0..1usize << self.n {
            let mut weight = 1.0f32;
            let mut index = base;
            for axis in 0..self.n {
                if corner >> axis & 1 == 1 {
                    weight *= fracs[axis];
                    index += strides[axis];
                } else {
                    weight *= 1.0 - fracs[axis];
                }
            }
            if weight != 0.0 {
                for (sum, &value) in acc.iter_mut().zip(&self.rgb[index]) {
                    *sum += weight * value as f32;
                }
            }
        }
        acc.map(|value| value.round().clamp(0.0, 255.0) as u8)
    }
}

impl<'a> PixelColor<'a> {
    /// Pick the conversion for an image of `pixels` pixels. Tables and caches
    /// cost up-front work (a tint table evaluates the transform 256 times),
    /// so small images — tiny inline images are common — convert directly.
    fn new(cs: &'a ResolvedColorSpace, pixels: usize) -> Self {
        match cs {
            ResolvedColorSpace::Indexed {
                base,
                hival,
                lookup,
            } if pixels > *hival as usize
                || !matches!(base.as_ref(), ResolvedColorSpace::Tint { .. }) =>
            {
                Self::Table(build_indexed_rgb_palette(base, *hival, lookup))
            }
            ResolvedColorSpace::Tint { n: 1, .. } if pixels > 256 => {
                Self::Table((0..=255u8).map(|t| components_to_rgb(cs, &[t])).collect())
            }
            _ if pixels < PIXEL_CACHE_SLOTS => Self::Direct(cs),
            ResolvedColorSpace::Lab { .. }
            | ResolvedColorSpace::CalGray(_)
            | ResolvedColorSpace::CalRGB(_) => Self::CachedNarrow {
                cs,
                slots: vec![(0, [0; 3]); PIXEL_CACHE_SLOTS],
                miss: Miss::Exact(CacheStats::default()),
            },
            ResolvedColorSpace::Tint { n: 2..=4, .. } => Self::CachedNarrow {
                cs,
                slots: vec![(0, [0; 3]); PIXEL_CACHE_SLOTS],
                miss: Miss::Tint(TintBudget::default()),
            },
            ResolvedColorSpace::Tint { .. } => Self::CachedWide {
                cs,
                slots: vec![None; PIXEL_CACHE_SLOTS],
                miss: Miss::Tint(TintBudget::default()),
            },
            _ => Self::Direct(cs),
        }
    }

    /// Convert one pixel's decoded 8-bit components (`cs.components()` of
    /// them; an Indexed index beyond `hival` reads the last entry).
    #[inline]
    fn convert(&mut self, comps: &[u8]) -> [u8; 3] {
        let (rgb, drop_cache) = match self {
            Self::Direct(cs) => return components_to_rgb(cs, comps),
            Self::Table(table) => {
                return table
                    .get(comps[0] as usize)
                    .or(table.last())
                    .copied()
                    .unwrap_or([0; 3])
            }
            Self::CachedNarrow { cs, slots, miss } => {
                let packed = comps
                    .iter()
                    .fold(0u32, |key, &byte| (key << 8) | byte as u32);
                let key = (1u64 << 32) | packed as u64;
                // Fibonacci hashing: the top bits of the product pick the slot.
                let index = (packed.wrapping_mul(0x9E37_79B1) >> (32 - PIXEL_CACHE_BITS)) as usize;
                let slot = &mut slots[index];
                if slot.0 == key {
                    miss.hit();
                    return slot.1;
                }
                let (rgb, drop_cache) = miss.convert(cs, comps);
                *slot = (key, rgb);
                (rgb, drop_cache)
            }
            Self::CachedWide { cs, slots, miss } => {
                let mut key = [0u8; MAX_COMPONENTS];
                key[..comps.len()].copy_from_slice(comps);
                // FNV-1a over the tuple picks the slot.
                let hash = comps.iter().fold(0x811c_9dc5u32, |hash, &byte| {
                    (hash ^ byte as u32).wrapping_mul(0x0100_0193)
                });
                let slot = &mut slots[hash as usize & (PIXEL_CACHE_SLOTS - 1)];
                if let Some((cached, rgb)) = slot {
                    if *cached == key {
                        miss.hit();
                        return *rgb;
                    }
                }
                let (rgb, drop_cache) = miss.convert(cs, comps);
                *slot = Some((key, rgb));
                (rgb, drop_cache)
            }
        };
        if drop_cache {
            if let Self::CachedNarrow { cs, .. } | Self::CachedWide { cs, .. } = self {
                *self = Self::Direct(cs);
            }
        }
        rgb
    }
}

/// Convert one pixel's decoded 8-bit components to RGB. For Indexed spaces,
/// `comps[0]` is the palette index (clamped to `hival`); for
/// Separation/DeviceN they are tints scaled to 0..=255. Missing components
/// read as 0.
fn components_to_rgb(cs: &ResolvedColorSpace, comps: &[u8]) -> [u8; 3] {
    let get = |i: usize| comps.get(i).copied().unwrap_or(0);
    match cs {
        ResolvedColorSpace::CalGray(_) | ResolvedColorSpace::CalRGB(_) => float_components_to_rgb(
            cs,
            &[
                get(0) as f64 / 255.0,
                get(1) as f64 / 255.0,
                get(2) as f64 / 255.0,
            ],
        ),
        ResolvedColorSpace::Gray => [get(0); 3],
        ResolvedColorSpace::Rgb => [get(0), get(1), get(2)],
        ResolvedColorSpace::Cmyk => cmyk_to_rgb(&[get(0), get(1), get(2), get(3)]),
        ResolvedColorSpace::Lab { white_point, range } => {
            let l = get(0) as f64 / 255.0 * 100.0;
            let a = range[0] + get(1) as f64 / 255.0 * (range[1] - range[0]);
            let b = range[2] + get(2) as f64 / 255.0 * (range[3] - range[2]);
            let (r, g, b) = zpdf_color::lab_to_rgb(l, a, b, *white_point);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        // Whole images take the buffer-level path in decode_raw_samples_icc;
        // this per-pixel call serves Indexed bases built without the
        // interpreter's palette pre-baking.
        ResolvedColorSpace::Icc { transform, .. } => {
            transform.comps8_to_rgb8(&[get(0), get(1), get(2), get(3)])
        }
        ResolvedColorSpace::Indexed {
            base,
            hival,
            lookup,
        } => {
            let n = base.components().min(MAX_COMPONENTS);
            let off = get(0).min(*hival) as usize * n;
            let mut bc = [0u8; MAX_COMPONENTS];
            for (i, b) in bc.iter_mut().enumerate().take(n) {
                // A short palette reads as 0 rather than failing the image.
                *b = lookup.get(off + i).copied().unwrap_or(0);
            }
            // Recursion terminates: `base` is a strictly smaller tree (and an
            // Indexed base of Indexed is illegal per spec 8.6.6.3 anyway).
            components_to_rgb(base, &bc[..n])
        }
        ResolvedColorSpace::Tint {
            n,
            transform,
            alternate,
        } => {
            let n = (*n as usize).min(MAX_COMPONENTS);
            let mut tints = [0.0f64; MAX_COMPONENTS];
            for (i, tint) in tints.iter_mut().enumerate().take(n) {
                *tint = get(i) as f64 / 255.0;
            }
            tint_to_rgb(transform.as_deref(), alternate, &tints[..n])
        }
    }
}

/// Map colorant tints through a Separation/DeviceN tint transform into its
/// alternate space. A missing transform, an evaluation error or too few
/// outputs falls back to `gray(1 − max tint)` (full ink = black).
fn tint_to_rgb(
    transform: Option<&zpdf_color::PdfFunction>,
    alternate: &ResolvedColorSpace,
    tints: &[f64],
) -> [u8; 3] {
    if let Some(out) = transform.and_then(|f| f.eval(tints)) {
        if out.len() >= alternate.components() {
            return float_components_to_rgb(alternate, &out);
        }
    }
    let max_tint = tints
        .iter()
        .fold(0.0f64, |max, &tint| max.max(tint.clamp(0.0, 1.0)));
    [unit_to_u8(1.0 - max_tint); 3]
}

/// Convert colour components in a space's natural ranges (0..=1 for device
/// and ICC spaces, L*a*b* values for Lab) to RGB — the form tint-transform
/// outputs and `/Matte` entries take. Special spaces cannot appear here
/// (validation rejects them as alternates); they map to black.
fn float_components_to_rgb(cs: &ResolvedColorSpace, vals: &[f64]) -> [u8; 3] {
    let get = |i: usize| vals.get(i).copied().unwrap_or(0.0);
    match cs {
        ResolvedColorSpace::CalGray(params) => {
            let (r, g, b) = params.to_rgb(get(0));
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        ResolvedColorSpace::CalRGB(params) => {
            let (r, g, b) = params.to_rgb([get(0), get(1), get(2)]);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        ResolvedColorSpace::Gray => [unit_to_u8(get(0)); 3],
        ResolvedColorSpace::Rgb => [unit_to_u8(get(0)), unit_to_u8(get(1)), unit_to_u8(get(2))],
        ResolvedColorSpace::Cmyk => {
            let (r, g, b) = zpdf_color::cmyk_to_rgb(get(0), get(1), get(2), get(3));
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        ResolvedColorSpace::Lab { white_point, range } => {
            let l = get(0).clamp(0.0, 100.0);
            let a = get(1).clamp(range[0], range[1]);
            let b = get(2).clamp(range[2], range[3]);
            let (r, g, b) = zpdf_color::lab_to_rgb(l, a, b, *white_point);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        ResolvedColorSpace::Icc { transform, .. } => {
            let (r, g, b) = transform.color_to_rgb(vals);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)]
        }
        ResolvedColorSpace::Indexed { .. } | ResolvedColorSpace::Tint { .. } => [0; 3],
    }
}

/// `0.0..=1.0` → `0..=255`, rounding; NaN reads as 0.
#[inline]
fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn build_indexed_rgb_palette(base: &ResolvedColorSpace, hival: u8, lookup: &[u8]) -> Vec<[u8; 3]> {
    let n = base.components().min(MAX_COMPONENTS);
    (0..=hival)
        .map(|index| {
            let offset = index as usize * n;
            let mut comps = [0u8; MAX_COMPONENTS];
            for (component, value) in comps.iter_mut().take(n).enumerate() {
                *value = lookup.get(offset + component).copied().unwrap_or(0);
            }
            components_to_rgb(base, &comps[..n])
        })
        .collect()
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
    for (px, &a) in image.data.as_chunks_mut::<4>().0.iter_mut().zip(plane) {
        multiply_pixel_alpha(px, a);
    }
    image.has_alpha = true;
    image.premultiplied = true;
}

#[inline]
fn multiply_pixel_alpha(pixel: &mut [u8; 4], alpha: u8) {
    // `mul255(v, 255) == v` for every `v`, and an opaque mask pixel is the
    // common case in a soft mask — skipping it turns four multiplies into a
    // predictable branch, with identical bytes either way.
    if alpha == 255 {
        return;
    }
    pixel[0] = mul255(pixel[0], alpha);
    pixel[1] = mul255(pixel[1], alpha);
    pixel[2] = mul255(pixel[2], alpha);
    pixel[3] = mul255(pixel[3], alpha);
}

/// `round(v * a / 255)` without going through floats.
#[inline]
fn mul255(v: u8, a: u8) -> u8 {
    ((v as u32 * a as u32 + 127) / 255) as u8
}

/// Bilinear resample of a single-channel plane (fits mask planes onto the
/// image they soften). Missing source data reads as opaque (255).
fn resample_bilinear(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let Some(out_size) = (dw as usize).checked_mul(dh as usize) else {
        tracing::warn!("bilinear mask resample output size overflow");
        return Vec::new();
    };

    let (sw_u, sh_u) = (sw as usize, sh as usize);
    let sample = |x: usize, y: usize| -> f64 {
        y.checked_mul(sw_u)
            .and_then(|offset| offset.checked_add(x))
            .and_then(|offset| src.get(offset))
            .copied()
            .unwrap_or(255) as f64
    };
    let mut out = Vec::with_capacity(out_size);
    let x_scale = sw as f64 / dw as f64;
    let y_scale = sh as f64 / dh as f64;
    for dy in 0..dh {
        let fy = ((dy as f64 + 0.5) * y_scale - 0.5).clamp(0.0, (sh - 1) as f64);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh_u - 1);
        let ty = fy - y0 as f64;
        for dx in 0..dw {
            let fx = ((dx as f64 + 0.5) * x_scale - 0.5).clamp(0.0, (sw - 1) as f64);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw_u - 1);
            let tx = fx - x0 as f64;
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

    #[test]
    fn direct_calgray_dictionary_is_not_inferred_as_device_gray() {
        let mut params = PdfDict::new();
        params.insert(zpdf_core::PdfName::new("Gamma"), PdfObject::Integer(2));
        let mut dict = PdfDict::new();
        for key in ["Width", "Height"] {
            dict.insert(zpdf_core::PdfName::new(key), PdfObject::Integer(1));
        }
        dict.insert(
            zpdf_core::PdfName::new("BitsPerComponent"),
            PdfObject::Integer(8),
        );
        dict.insert(
            zpdf_core::PdfName::new("ColorSpace"),
            PdfObject::Array(vec![
                PdfObject::Name(zpdf_core::PdfName::new("CalGray")),
                PdfObject::Dict(params),
            ]),
        );
        let image = decode_image_xobject(&[128], &dict).unwrap();
        assert!(
            image.data[..3]
                .iter()
                .all(|v| (i32::from(*v) - 138).abs() <= 1),
            "{:?}",
            image.data
        );
    }
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
    fn image_ids_wrap_without_overwriting_live_entries() {
        let image = DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 255],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        let mut cache = ImageCache::new();
        cache.next_id = u32::MAX;
        assert_eq!(cache.insert(image.clone()), u32::MAX);
        assert_eq!(cache.insert(image), 0);
        assert_eq!(cache.images.len(), 2);
    }

    #[test]
    fn image_cache_budget_rejects_without_evicting_or_advancing_id() {
        let image = DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 255],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        let mut cache = ImageCache::new();
        assert_eq!(cache.try_insert_with_limit(image.clone(), 4), Some(0));
        assert_eq!(cache.try_insert_with_limit(image.clone(), 7), None);
        assert_eq!(cache.bytes_used(), 4);
        assert!(cache.get(0).is_some());
        assert_eq!(cache.try_insert_with_limit(image, 8), Some(1));
    }

    #[test]
    fn removing_an_image_releases_its_byte_accounting() {
        let image = DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 255],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        let mut cache = ImageCache::new();
        let id = cache.insert(image);
        assert_eq!(cache.bytes_used(), 4);
        assert!(cache.remove(id).is_some());
        assert_eq!(cache.bytes_used(), 0);
        assert!(cache.is_empty());
        assert!(cache.remove(id).is_none());
    }

    #[test]
    fn image_cache_charges_retained_vector_capacity() {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&[0, 0, 0, 255]);
        assert!(data.capacity() > data.len());
        let retained_bytes = data.capacity() as u64;
        let image = DecodedImage {
            width: 1,
            height: 1,
            data,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };

        let mut cache = ImageCache::new();
        assert_eq!(cache.try_insert_with_limit(image, 4), None);
        assert_eq!(cache.bytes_used(), 0);
        assert!(cache.images.is_empty());

        let mut data = Vec::with_capacity(retained_bytes as usize);
        data.extend_from_slice(&[0, 0, 0, 255]);
        let admitted_bytes = data.capacity() as u64;
        let image = DecodedImage {
            width: 1,
            height: 1,
            data,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        assert_eq!(cache.try_insert_with_limit(image, admitted_bytes), Some(0));
        assert_eq!(cache.bytes_used(), admitted_bytes);
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
    fn raw_and_dct_images_honor_custom_pixel_limit() {
        let limits = ParseLimits {
            max_image_pixels: 3,
            ..ParseLimits::default()
        };
        let raw = image_dict(2, 2, 8, Some("DeviceGray"));
        assert!(decode_image_xobject_with_limits(&[0; 4], &raw, &limits).is_err());

        let mut dct = raw;
        dct.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        assert!(decode_image_xobject_with_limits(&[0; 4], &dct, &limits).is_err());
    }

    #[test]
    fn image_and_stencil_masks_honor_custom_pixel_limit() {
        let limits = ParseLimits {
            max_image_pixels: 3,
            ..ParseLimits::default()
        };
        let mut mask = image_dict(2, 2, 1, None);
        mask.insert(PdfName::new("ImageMask"), PdfObject::Bool(true));
        assert!(decode_image_xobject_with_limits(&[0], &mask, &limits).is_err());

        let mut image = DecodedImage {
            width: 1,
            height: 1,
            data: vec![10, 20, 30, 255],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        };
        apply_stencil_mask_with_limits(&mut image, &[0], 2, 2, false, &limits);
        assert_eq!(image.data, [10, 20, 30, 255]);
        assert!(!image.has_alpha);
    }

    #[test]
    fn dct_grayscale_accepts_exact_one_byte_per_pixel() {
        let mut dict = image_dict(2, 1, 8, Some("DeviceGray"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img = decode_image_xobject(&[10, 200], &dict).unwrap();
        assert_eq!(img.data, [10, 10, 10, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn wrapped_dimensions_and_bpc_are_rejected() {
        assert!(decode_image_xobject(&[], &image_dict(-1, 1, 8, Some("DeviceGray"))).is_err());
        assert!(decode_image_xobject(
            &[],
            &image_dict(i64::from(u32::MAX) + 1, 1, 8, Some("DeviceGray"))
        )
        .is_err());
        assert!(decode_image_xobject(&[0], &image_dict(1, 1, 257, Some("DeviceGray"))).is_err());
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
        // An explicit non-JPX filter owns the bytes; ignore a signature that
        // happens to appear after that filter's output.
        let mut flate = image_dict(1, 1, 8, None);
        flate.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("FlateDecode")),
        );
        assert!(!is_jpx_encoded(&flate, b"\xFF\x4F\xFF\x51rest"));
        // An empty filter array declares nothing, so sniffing still applies.
        let mut empty = image_dict(1, 1, 8, None);
        empty.insert(PdfName::new("Filter"), PdfObject::Array(Vec::new()));
        assert!(is_jpx_encoded(&empty, b"\xFF\x4F\xFF\x51rest"));
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
    fn lab_image_uses_its_white_point_and_range() {
        let mut params = PdfDict::new();
        params.insert(
            PdfName::new("WhitePoint"),
            PdfObject::Array(vec![
                PdfObject::Real(0.9642),
                PdfObject::Real(1.0),
                PdfObject::Real(0.8249),
            ]),
        );
        let mut dict = image_dict(1, 1, 8, None);
        dict.insert(
            PdfName::new("ColorSpace"),
            PdfObject::Array(vec![
                PdfObject::Name(PdfName::new("Lab")),
                PdfObject::Dict(params),
            ]),
        );
        // L*=100, a*/b* nearest representable neutral under default Range.
        let img = decode_image_xobject(&[255, 128, 128], &dict).unwrap();
        let px = pixel(&img, 0);
        assert!(px[0] > 248 && px[1] > 248 && px[2] > 248, "{px:?}");
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

    /// The 8x8 pure-cyan CMYK JPEG written by Pillow (Adobe APP14, transform
    /// 0, Adobe-inverted samples) run through the parser's real DCTDecode
    /// filter, plus the image dict Pillow's PDF writer pairs it with
    /// (`/Decode [1 0 1 0 1 0 1 0]` undoes the inversion).
    fn cmyk_cyan_jpeg() -> (Vec<u8>, PdfDict) {
        let mut dict = image_dict(8, 8, 8, Some("DeviceCMYK"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let jpeg = include_bytes!("testdata/cmyk_cyan_8x8.jpg");
        let decoded = zpdf_parser::filters::decode_stream(jpeg, &dict).expect("DCTDecode");
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0, 1, 0, 1, 0, 1, 0]));
        (decoded, dict)
    }

    fn to_u8(value: f64) -> u8 {
        (value * 255.0).round() as u8
    }

    fn assert_near(actual: &[u8], expected: [u8; 3], tolerance: i16) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                (*a as i16 - e as i16).abs() <= tolerance,
                "{actual:?} not within {tolerance} of {expected:?}"
            );
        }
    }

    #[test]
    fn cmyk_jpeg_decodes_to_cyan() {
        let (decoded, dict) = cmyk_cyan_jpeg();
        // The filter hands over raw CMYK, not zune's naïve RGB.
        assert_eq!(decoded.len(), 8 * 8 * 4);
        let img = decode_image_xobject_resolved(
            &decoded,
            &dict,
            [0, 0, 0],
            Some(ResolvedColorSpace::Cmyk),
        )
        .unwrap();
        // 100 % cyan through the Adobe DeviceCMYK polynomial ≈ (0, 184, 242),
        // not the naïve pure (0, 255, 255); JPEG loss allows a few levels.
        let (r, g, b) = zpdf_color::cmyk_to_rgb(1.0, 0.0, 0.0, 0.0);
        let px = pixel(&img, 0);
        assert_near(px, [to_u8(r), to_u8(g), to_u8(b)], 6);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn ycck_jpeg_cmyk_goes_through_the_cmyk_conversion() {
        // A CMYK sample from a 4-component JPEG converts exactly like a raw
        // DeviceCMYK sample (here the K-only near-black).
        let mut dict = image_dict(1, 1, 8, Some("DeviceCMYK"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img = decode_image_xobject(&[0, 0, 0, 255], &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[44, 46, 53, 255]);
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
        // the LUT profile: the DCT sniffer must keep the ICC space, so the
        // pixel is the profile's rendering of the JPEG's own CMYK sample (the
        // old 3-byte zune output silently skipped the transform).
        let (decoded, dict) = cmyk_cyan_jpeg();
        let cs = icc_cs(include_bytes!("testdata/cmyk_lut.icc"));
        let ResolvedColorSpace::Icc { transform, .. } = cs.clone() else {
            unreachable!()
        };
        let sample: [u8; 4] = std::array::from_fn(|c| 255 - decoded[c]);
        let expected = transform.comps8_to_rgb8(&sample);
        let img = decode_image_xobject_resolved(&decoded, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(&pixel(&img, 0)[..3], &expected);
        // …which is the LUT profile's cyan, not the polynomial's.
        assert_near(&transform.comps8_to_rgb8(&[255, 0, 0, 0]), expected, 6);
    }

    // ---- Separation / DeviceN images ----

    fn real_array(vals: &[f64]) -> PdfObject {
        PdfObject::Array(vals.iter().map(|&v| PdfObject::Real(v)).collect())
    }

    /// FunctionType 2 dict `x → C0 + x·(C1 − C0)`.
    fn exponential(c0: &[f64], c1: &[f64]) -> PdfObject {
        let mut d = PdfDict::new();
        d.insert(PdfName::new("FunctionType"), PdfObject::Integer(2));
        d.insert(PdfName::new("Domain"), real_array(&[0.0, 1.0]));
        d.insert(PdfName::new("C0"), real_array(c0));
        d.insert(PdfName::new("C1"), real_array(c1));
        d.insert(PdfName::new("N"), PdfObject::Real(1.0));
        PdfObject::Dict(d)
    }

    fn parsed(function: &PdfObject) -> std::sync::Arc<zpdf_color::PdfFunction> {
        std::sync::Arc::new(zpdf_color::PdfFunction::parse_object(function, &mut |_| None).unwrap())
    }

    /// `[/Separation /Black /DeviceCMYK {t → (0 0 0 t)}]`.
    fn black_separation() -> ResolvedColorSpace {
        ResolvedColorSpace::Tint {
            n: 1,
            transform: Some(parsed(&exponential(&[0.0; 4], &[0.0, 0.0, 0.0, 1.0]))),
            alternate: Box::new(ResolvedColorSpace::Cmyk),
        }
    }

    fn name(n: &str) -> PdfObject {
        PdfObject::Name(PdfName::new(n))
    }

    #[test]
    fn separation_image_maps_tints_through_the_alternate() {
        // Tint 0 = no ink (white), tint 1 = full black ink — the reverse of
        // reading the samples as DeviceGray.
        let dict = image_dict(2, 1, 8, None);
        let img =
            decode_image_xobject_resolved(&[0, 255], &dict, [0, 0, 0], Some(black_separation()))
                .unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[44, 46, 53, 255]);

        // 1 bpc with the default /Decode [0 1]: bit 1 is full tint.
        let dict = image_dict(2, 1, 1, None);
        let img = decode_image_xobject_resolved(
            &[0b0100_0000],
            &dict,
            [0, 0, 0],
            Some(black_separation()),
        )
        .unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[44, 46, 53, 255]);

        // /Decode [1 0] inverts the tint.
        let mut dict = image_dict(1, 1, 8, None);
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0]));
        let img = decode_image_xobject_resolved(&[255], &dict, [0, 0, 0], Some(black_separation()))
            .unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
    }

    #[test]
    fn inline_separation_without_resolution_keeps_polarity() {
        // The dict-only inference path (no caller resolution): an inline
        // type-2 tint transform is evaluated, and a missing one falls back to
        // `gray(1 − tint)` rather than plain DeviceGray.
        let mut dict = image_dict(2, 1, 8, None);
        dict.insert(
            PdfName::new("ColorSpace"),
            PdfObject::Array(vec![
                name("Separation"),
                name("Black"),
                name("DeviceCMYK"),
                exponential(&[0.0; 4], &[0.0, 0.0, 0.0, 1.0]),
            ]),
        );
        let img = decode_image_xobject(&[0, 255], &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[44, 46, 53, 255]);

        dict.insert(
            PdfName::new("ColorSpace"),
            PdfObject::Array(vec![
                name("Separation"),
                name("Spot"),
                name("DeviceGray"),
                PdfObject::Null,
            ]),
        );
        let img = decode_image_xobject(&[0, 255], &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 255]);
    }

    #[test]
    fn devicen_image_is_sized_by_colorant_count() {
        // [/DeviceN [/Cyan /Magenta] /DeviceCMYK {c m → (c m 0 0)}]: two
        // components per pixel (the old gray fallback read one).
        let mut f = PdfDict::new();
        f.insert(PdfName::new("FunctionType"), PdfObject::Integer(4));
        f.insert(PdfName::new("Domain"), real_array(&[0.0, 1.0, 0.0, 1.0]));
        f.insert(PdfName::new("Range"), real_array(&[0.0, 1.0].repeat(4)));
        let transform = zpdf_color::PdfFunction::parse(&f, Some(b"{ 0 0 }"), &mut |_| None)
            .map(std::sync::Arc::new);
        assert!(transform.is_some());
        let cs = ResolvedColorSpace::Tint {
            n: 2,
            transform,
            alternate: Box::new(ResolvedColorSpace::Cmyk),
        };
        let dict = image_dict(3, 1, 8, None);
        let samples = [255, 0, 0, 255, 0, 0];
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(&pixel(&img, 0)[..3], &cmyk_to_rgb(&[255, 0, 0, 0])); // cyan
        assert_eq!(&pixel(&img, 1)[..3], &cmyk_to_rgb(&[0, 255, 0, 0])); // magenta
        assert_eq!(pixel(&img, 2), &[255, 255, 255, 255]); // no ink
    }

    #[test]
    fn tint_tables_and_caches_match_direct_conversion() {
        // Large enough images take the 256-entry tint table (Separation) and
        // the tuple cache (DeviceN, 6 colorants → wide keys); both must equal
        // the per-pixel conversion small images use.
        let separation = black_separation();
        let samples: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
        let dict = image_dict(512, 1, 8, None);
        let img =
            decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(separation.clone()))
                .unwrap();
        for (px, &s) in img.data.as_chunks::<4>().0.iter().zip(&samples) {
            assert_eq!(px[..3], components_to_rgb(&separation, &[s]));
        }

        let mut f = PdfDict::new();
        f.insert(PdfName::new("FunctionType"), PdfObject::Integer(4));
        f.insert(PdfName::new("Domain"), real_array(&[0.0, 1.0].repeat(6)));
        f.insert(PdfName::new("Range"), real_array(&[0.0, 1.0].repeat(4)));
        // (c m y k s1 s2) → (c m y max(k, s1, s2)).
        let program = b"{ 2 copy lt { exch } if pop 2 copy lt { exch } if pop }";
        let devicen = ResolvedColorSpace::Tint {
            n: 6,
            transform: zpdf_color::PdfFunction::parse(&f, Some(program), &mut |_| None)
                .map(std::sync::Arc::new),
            alternate: Box::new(ResolvedColorSpace::Cmyk),
        };
        assert!(matches!(
            &devicen,
            ResolvedColorSpace::Tint {
                transform: Some(_),
                ..
            }
        ));
        let (w, h) = (128usize, 64usize);
        let samples: Vec<u8> = (0..w * h * 6).map(|i| (i * 7 % 13 * 19) as u8).collect();
        let dict = image_dict(w as i64, h as i64, 8, None);
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(devicen.clone()))
            .unwrap();
        for (px, src) in img
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .zip(samples.as_chunks::<6>().0)
        {
            assert_eq!(px[..3], components_to_rgb(&devicen, src));
        }
    }

    /// Type 4 `n`-in tint transform from a PostScript program.
    fn calculator(
        n: usize,
        outputs: usize,
        program: &[u8],
    ) -> std::sync::Arc<zpdf_color::PdfFunction> {
        let mut f = PdfDict::new();
        f.insert(PdfName::new("FunctionType"), PdfObject::Integer(4));
        f.insert(PdfName::new("Domain"), real_array(&[0.0, 1.0].repeat(n)));
        f.insert(
            PdfName::new("Range"),
            real_array(&[0.0, 1.0].repeat(outputs)),
        );
        std::sync::Arc::new(
            zpdf_color::PdfFunction::parse(&f, Some(program), &mut |_| None).unwrap(),
        )
    }

    #[test]
    fn noisy_devicen_image_bounds_tint_evaluations() {
        // 65 536 distinct 3-colorant tuples exceed the exact-evaluation budget;
        // the rest interpolates a sampled grid. The transform here is linear
        // (t → 1 − t into RGB), so interpolation is exact up to rounding.
        let cs = ResolvedColorSpace::Tint {
            n: 3,
            transform: Some(calculator(
                3,
                3,
                b"{ 1 exch sub 3 1 roll 1 exch sub 3 1 roll 1 exch sub 3 1 roll }",
            )),
            alternate: Box::new(ResolvedColorSpace::Rgb),
        };
        let samples: Vec<u8> = (0..256usize * 256)
            .flat_map(|i| {
                let (x, y) = (i % 256, i / 256);
                [x as u8, y as u8, (x + y) as u8]
            })
            .collect();
        let dict = image_dict(256, 256, 8, None);
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        for (px, src) in img
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .zip(samples.as_chunks::<3>().0)
        {
            for (&got, &tint) in px[..3].iter().zip(src) {
                assert!(
                    (got as i16 - (255 - tint) as i16).abs() <= 1,
                    "{px:?} for {src:?}"
                );
            }
        }
    }

    #[test]
    fn two_colorant_tint_grid_is_exact() {
        // 256 samples per axis: every 8-bit tuple is a grid point, so the
        // past-budget path reproduces the exact conversion even for a
        // non-linear transform.
        let transform = calculator(2, 4, b"{ dup mul exch dup mul exch 0 0 }");
        let grid = TintGrid::build(Some(&transform), &ResolvedColorSpace::Cmyk, 2).unwrap();
        assert_eq!(grid.side, 256);
        for comps in [[0u8, 0], [17, 200], [128, 128], [255, 3], [255, 255]] {
            let tints = comps.map(|c| c as f64 / 255.0);
            assert_eq!(
                grid.lookup(&comps),
                tint_to_rgb(Some(&transform), &ResolvedColorSpace::Cmyk, &tints)
            );
        }
        assert!(TintGrid::build(None, &ResolvedColorSpace::Cmyk, 7).is_none());
    }

    #[test]
    fn devicen_image_with_many_colorants_and_no_transform() {
        // Six colorants exceed the old four-component scratch arrays; with no
        // usable transform each pixel is gray(1 − max tint).
        let cs = ResolvedColorSpace::Tint {
            n: 6,
            transform: None,
            alternate: Box::new(ResolvedColorSpace::Cmyk),
        };
        let dict = image_dict(2, 1, 4, None);
        // 4 bpc: pixel 0 all zero; pixel 1 has only the sixth colorant at 15.
        let samples = [0x00, 0x00, 0x00, 0x00, 0x00, 0x0F];
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 255]);
    }

    #[test]
    fn indexed_image_with_separation_base() {
        // Palette entries are tints: index 0 → no ink, index 1 → full ink.
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(black_separation()),
            hival: 1,
            lookup: vec![0, 255],
        };
        let dict = image_dict(2, 1, 8, None);
        let img = decode_image_xobject_resolved(&[0, 1], &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[44, 46, 53, 255]);
    }

    #[test]
    fn indexed_image_with_lab_base() {
        // Lab palette bytes scale to L* 0..100 and a*/b* over the Range.
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(ResolvedColorSpace::Lab {
                white_point: [0.9642, 1.0, 0.8249],
                range: [-128.0, 127.0, -128.0, 127.0],
            }),
            hival: 1,
            lookup: vec![0, 128, 128, 255, 128, 128],
        };
        let dict = image_dict(2, 1, 8, None);
        let img = decode_image_xobject_resolved(&[0, 1], &dict, [0, 0, 0], Some(cs)).unwrap();
        let (black, white) = (pixel(&img, 0), pixel(&img, 1));
        assert!(black[..3].iter().all(|&v| v < 8), "{black:?}");
        assert!(white[..3].iter().all(|&v| v > 247), "{white:?}");
    }

    #[test]
    fn separation_gray_jpeg_keeps_the_tint_space() {
        // A 1-component JPEG under a Separation space is a tint image, not
        // DeviceGray.
        let mut dict = image_dict(2, 1, 8, None);
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img =
            decode_image_xobject_resolved(&[0, 255], &dict, [0, 0, 0], Some(black_separation()))
                .unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[44, 46, 53, 255]);
    }

    #[test]
    fn special_alternate_spaces_are_rejected() {
        let cs = ResolvedColorSpace::Tint {
            n: 1,
            transform: None,
            alternate: Box::new(black_separation()),
        };
        let dict = image_dict(1, 1, 8, None);
        assert!(decode_image_xobject_resolved(&[0], &dict, [0, 0, 0], Some(cs)).is_err());
    }

    // ---- short data, DCT geometry, /BitsPerComponent ----

    #[test]
    fn short_raw_data_pads_missing_samples_with_zero() {
        // 4x4 gray with 15 of 16 bytes: the last pixel reads sample 0.
        let dict = image_dict(4, 4, 8, Some("DeviceGray"));
        let img = decode_image_xobject(&[200; 15], &dict).unwrap();
        assert_eq!(pixel(&img, 14), &[200, 200, 200, 255]);
        assert_eq!(pixel(&img, 15), &[0, 0, 0, 255]);
        // Sub-byte slow path too.
        let dict = image_dict(8, 2, 1, Some("DeviceGray"));
        let img = decode_image_xobject(&[0xFF], &dict).unwrap();
        assert_eq!(pixel(&img, 7), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 8), &[0, 0, 0, 255]);
        // Less than one row is still no image.
        let dict = image_dict(4, 4, 8, Some("DeviceGray"));
        assert!(decode_image_xobject(&[200; 3], &dict).is_err());
    }

    #[test]
    fn short_icc_data_pads_missing_samples_with_zero() {
        let cs = icc_cs(include_bytes!("testdata/gray_linear.icc"));
        let dict = image_dict(2, 2, 8, None);
        let img =
            decode_image_xobject_resolved(&[255, 255, 255], &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 2), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 3), &[0, 0, 0, 255]);
    }

    #[test]
    fn dct_output_with_other_dimensions_adopts_them() {
        // Dict says 4x4 RGB but the JPEG decoded to 4x5: keep the JPEG's rows.
        let mut dict = image_dict(4, 4, 8, Some("DeviceRGB"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img = decode_image_xobject(&[128; 60], &dict).unwrap();
        assert_eq!((img.width, img.height), (4, 5));
        assert_eq!(pixel(&img, 19), &[128, 128, 128, 255]);
        // Width disagreement with a matching height.
        let img = decode_image_xobject(&[64; 3 * 4 * 6], &dict).unwrap();
        assert_eq!((img.width, img.height), (4, 6));
        let mut tall = image_dict(3, 4, 8, Some("DeviceGray"));
        tall.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img = decode_image_xobject(&[9; 5 * 4], &tall).unwrap();
        assert_eq!((img.width, img.height), (5, 4));
        // Data that fits no geometry still fails.
        assert!(decode_image_xobject(&[1; 7], &dict).is_err());
    }

    #[test]
    fn integral_real_bits_per_component_is_honored() {
        let mut dict = image_dict(8, 1, 1, Some("DeviceGray"));
        dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Real(1.0));
        let img = decode_image_xobject(&[0b1000_0000], &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 255]);
        dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Real(1.5));
        assert!(decode_image_xobject(&[0], &dict).is_err());
    }

    // ---- /SMask interplay ----

    #[test]
    fn smask_overrides_color_key_mask() {
        let mut dict = image_dict(2, 1, 8, Some("DeviceGray"));
        dict.insert(PdfName::new("Mask"), int_array(&[0, 0]));
        dict.insert(
            PdfName::new("SMask"),
            PdfObject::Ref(zpdf_core::ObjectId(9, 0)),
        );
        let img = decode_image_xobject(&[0, 200], &dict).unwrap();
        assert!(!img.has_alpha);
        assert_eq!(pixel(&img, 0), &[0, 0, 0, 255]);
    }

    #[test]
    fn matte_soft_mask_unpremultiplies_preblended_colour() {
        // Red pre-blended with a white matte at α = 0.5: c′ = 1 + 0.5·(c − 1)
        // → (255, 128, 128). Un-premultiplied it is red again, then the fold
        // premultiplies it to (128, 0, 0, 128).
        let dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        let preblended = [255, 128, 128, 255, 128, 128];
        let mask_dict = image_dict(2, 1, 8, Some("DeviceGray"));
        let mask = decode_image_xobject(&[128, 0], &mask_dict).unwrap();
        let mut smask_dict = mask_dict.clone();
        smask_dict.insert(PdfName::new("Matte"), int_array(&[1, 1, 1]));
        let matte = smask_matte(&smask_dict);
        assert_eq!(matte.as_deref(), Some(&[1.0, 1.0, 1.0][..]));

        let mut img = decode_image_xobject(&preblended, &dict).unwrap();
        apply_smask_image_with_matte(
            &mut img,
            &mask,
            matte.as_deref(),
            Some(&ResolvedColorSpace::Rgb),
        );
        assert!(img.premultiplied);
        // α is 128/255, not exactly ½, hence the 1-level slack.
        assert_near(pixel(&img, 0), [128, 0, 0], 1);
        assert_eq!(pixel(&img, 0)[3], 128);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]); // α = 0 stays transparent

        // A black matte (c′ = α·c), here with the colour space left out so the
        // single component reads as DeviceGray.
        let mut img = decode_image_xobject(&[128, 0, 64, 0, 0, 0], &dict).unwrap();
        apply_smask_image_with_matte(&mut img, &mask, Some(&[0.0]), None);
        assert_eq!(pixel(&img, 0), &[128, 0, 64, 128]);

        // No /Matte: identical to apply_smask_image.
        let mut plain = decode_image_xobject(&preblended, &dict).unwrap();
        let mut reference = plain.clone();
        apply_smask_image_with_matte(&mut plain, &mask, None, None);
        apply_smask_image(&mut reference, &mask);
        assert_eq!(plain.data, reference.data);
    }

    #[test]
    fn lab_image_cache_matches_direct_conversion() {
        // The Lab pixel cache must reproduce the uncached conversion exactly.
        let cs = ResolvedColorSpace::Lab {
            white_point: [0.9642, 1.0, 0.8249],
            range: [-100.0, 100.0, -100.0, 100.0],
        };
        let samples: Vec<u8> = (0..3 * 64 * 64).map(|i| (i * 37 % 251) as u8).collect();
        let dict = image_dict(64, 64, 8, None);
        let img =
            decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs.clone())).unwrap();
        for (px, src) in img
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .zip(samples.as_chunks::<3>().0)
        {
            assert_eq!(px[..3], components_to_rgb(&cs, src));
        }
    }

    // ---- ImageMask stencil (pre-existing behaviour) ----

    #[test]
    fn image_mask_paints_fill() {
        let mut dict = image_dict(2, 1, 1, None);
        dict.insert(PdfName::new("ImageMask"), PdfObject::Bool(true));
        let img = decode_image_xobject_with_fill(&[0b0100_0000], &dict, [10, 20, 30]).unwrap();
        assert_eq!(pixel(&img, 0), &[10, 20, 30, 255]); // sample 0 paints
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]);
        assert!(img.is_image_mask);
        assert!(img.premultiplied);
    }
}
