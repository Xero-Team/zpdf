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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
}

pub fn decode_image_xobject(decoded_data: &[u8], dict: &PdfDict) -> Result<DecodedImage> {
    decode_image_xobject_with_fill(decoded_data, dict, [0, 0, 0])
}

/// Decode an image XObject, painting `/ImageMask true` stencils in `fill_rgb`
/// (the current graphics-state fill colour). Non-mask images ignore `fill_rgb`.
pub fn decode_image_xobject_with_fill(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
) -> Result<DecodedImage> {
    let width = dict.get_i64("Width")? as u32;
    let height = dict.get_i64("Height")? as u32;
    let bpc = dict.get_i64("BitsPerComponent").unwrap_or(8) as u8;

    // Bound the RGBA allocation (4 bytes/pixel) against the ParseLimits default
    // so a crafted /Width × /Height cannot OOM — applies to every image path,
    // including the 1-bpp stencil below.
    const MAX_IMAGE_PIXELS: u64 = 100_000_000;
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

    let cs = parse_colorspace(dict);
    let is_dct = is_dct_encoded(dict);

    if is_dct {
        return decode_dct_image(decoded_data, width, height, cs);
    }

    match (cs, bpc) {
        (ImageColorSpace::DeviceRGB, 8) => samples_rgb8_to_rgba(decoded_data, width, height),
        (ImageColorSpace::DeviceGray, 8) => samples_gray8_to_rgba(decoded_data, width, height),
        (ImageColorSpace::DeviceCMYK, 8) => samples_cmyk8_to_rgba(decoded_data, width, height),
        (ImageColorSpace::DeviceGray, 1) => samples_gray1_to_rgba(decoded_data, width, height),
        _ => {
            tracing::warn!("unsupported image format: {cs:?} {bpc}bpc, treating as gray");
            samples_gray8_to_rgba(decoded_data, width, height)
        }
    }
}

pub fn apply_smask(image: &mut DecodedImage, mask_data: &[u8], mask_width: u32, mask_height: u32) {
    if mask_width != image.width || mask_height != image.height {
        return;
    }
    let pixel_count = (image.width * image.height) as usize;
    for (i, &m) in mask_data.iter().take(pixel_count).enumerate() {
        image.data[i * 4 + 3] = m;
    }
    image.has_alpha = true;
}

fn is_image_mask(dict: &PdfDict) -> bool {
    matches!(dict.get("ImageMask"), Some(PdfObject::Bool(true)))
}

/// An ImageMask's `/Decode [1 0]` inverts which sample value paints. Default
/// `[0 1]` means sample 0 paints (marks the page).
fn mask_decode_inverts(dict: &PdfDict) -> bool {
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

/// Build a stencil: paint `fill` (opaque) where the 1-bpp sample selects the
/// page, transparent elsewhere. `invert` flips the paint polarity (`/Decode
/// [1 0]`). Output alpha is 0 or 255, so the straight bytes are also valid
/// premultiplied RGBA for the rasterizer.
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
        premultiplied: false,
    })
}

fn parse_colorspace(dict: &PdfDict) -> ImageColorSpace {
    match dict.get("ColorSpace") {
        Some(PdfObject::Name(n)) => match n.as_str() {
            "DeviceRGB" => ImageColorSpace::DeviceRGB,
            "DeviceCMYK" => ImageColorSpace::DeviceCMYK,
            _ => ImageColorSpace::DeviceGray,
        },
        Some(PdfObject::Array(arr)) => {
            if let Some(PdfObject::Name(n)) = arr.first() {
                match n.as_str() {
                    "ICCBased" => {
                        // Guess from component count: the stream dict has /N
                        // but we don't have access to resolve it here.
                        // Fall back to RGB as most common ICC profile.
                        ImageColorSpace::DeviceRGB
                    }
                    "Indexed" | "I" => {
                        // Indexed colorspace — base is arr[1], treat as RGB for now
                        ImageColorSpace::DeviceRGB
                    }
                    "DeviceRGB" => ImageColorSpace::DeviceRGB,
                    "DeviceCMYK" => ImageColorSpace::DeviceCMYK,
                    "DeviceGray" => ImageColorSpace::DeviceGray,
                    _ => ImageColorSpace::DeviceGray,
                }
            } else {
                ImageColorSpace::DeviceGray
            }
        }
        _ => ImageColorSpace::DeviceRGB,
    }
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
    cs: ImageColorSpace,
) -> Result<DecodedImage> {
    // DCTDecode already decoded by the parser filter pipeline into raw samples.
    // The output is typically RGB (3 bytes/pixel) or Grayscale (1 byte/pixel).
    let pixel_count = (width * height) as usize;
    let expected_rgb = pixel_count * 3;
    let expected_cmyk = pixel_count * 4;

    if decoded_data.len() >= expected_cmyk && cs == ImageColorSpace::DeviceCMYK {
        samples_cmyk8_to_rgba(decoded_data, width, height)
    } else if decoded_data.len() >= expected_rgb {
        samples_rgb8_to_rgba(decoded_data, width, height)
    } else if decoded_data.len() >= pixel_count {
        samples_gray8_to_rgba(decoded_data, width, height)
    } else {
        Err(Error::StreamDecode(format!(
            "DCT decoded data too short: {} bytes for {}x{} image",
            decoded_data.len(),
            width,
            height
        )))
    }
}

fn samples_rgb8_to_rgba(data: &[u8], width: u32, height: u32) -> Result<DecodedImage> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for chunk in data.chunks(3).take(pixel_count) {
        if chunk.len() == 3 {
            rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 255]);
        }
    }
    // Pad remaining pixels if data is short
    while rgba.len() < pixel_count * 4 {
        rgba.extend_from_slice(&[0, 0, 0, 255]);
    }
    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: false,
        premultiplied: false,
    })
}

fn samples_gray8_to_rgba(data: &[u8], width: u32, height: u32) -> Result<DecodedImage> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for &g in data.iter().take(pixel_count) {
        rgba.extend_from_slice(&[g, g, g, 255]);
    }
    while rgba.len() < pixel_count * 4 {
        rgba.extend_from_slice(&[0, 0, 0, 255]);
    }
    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: false,
        premultiplied: false,
    })
}

fn samples_cmyk8_to_rgba(data: &[u8], width: u32, height: u32) -> Result<DecodedImage> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for chunk in data.chunks(4).take(pixel_count) {
        if chunk.len() == 4 {
            let c = chunk[0] as f32 / 255.0;
            let m = chunk[1] as f32 / 255.0;
            let y = chunk[2] as f32 / 255.0;
            let k = chunk[3] as f32 / 255.0;
            let r = ((1.0 - c) * (1.0 - k) * 255.0) as u8;
            let g = ((1.0 - m) * (1.0 - k) * 255.0) as u8;
            let b = ((1.0 - y) * (1.0 - k) * 255.0) as u8;
            rgba.extend_from_slice(&[r, g, b, 255]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 255]);
        }
    }
    while rgba.len() < pixel_count * 4 {
        rgba.extend_from_slice(&[0, 0, 0, 255]);
    }
    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: false,
        premultiplied: false,
    })
}

fn samples_gray1_to_rgba(data: &[u8], width: u32, height: u32) -> Result<DecodedImage> {
    let pixel_count = (width * height) as usize;
    let row_bytes = (width as usize).div_ceil(8);
    let mut rgba = Vec::with_capacity(pixel_count * 4);

    for row in 0..height as usize {
        for col in 0..width as usize {
            let byte_idx = row * row_bytes + col / 8;
            let bit_idx = 7 - (col % 8);
            let val = if byte_idx < data.len() {
                if (data[byte_idx] >> bit_idx) & 1 == 1 {
                    0u8 // 1-bit: 1 = black in PDF
                } else {
                    255u8 // 0 = white
                }
            } else {
                255
            };
            rgba.extend_from_slice(&[val, val, val, 255]);
        }
    }

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: false,
        premultiplied: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb8_to_rgba() {
        let samples = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 128, 128, 128];
        let img = samples_rgb8_to_rgba(&samples, 2, 2).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.data.len(), 16);
        assert_eq!(&img.data[0..4], &[255, 0, 0, 255]); // red
        assert_eq!(&img.data[4..8], &[0, 255, 0, 255]); // green
    }

    #[test]
    fn gray8_to_rgba() {
        let samples = vec![0, 128, 255, 64];
        let img = samples_gray8_to_rgba(&samples, 2, 2).unwrap();
        assert_eq!(&img.data[0..4], &[0, 0, 0, 255]);
        assert_eq!(&img.data[4..8], &[128, 128, 128, 255]);
    }

    #[test]
    fn gray1_to_rgba() {
        // 2x2 image: bits 1,0,0,1 → black,white,white,black
        let samples = vec![0b10000000, 0b01000000];
        let img = samples_gray1_to_rgba(&samples, 2, 2).unwrap();
        assert_eq!(&img.data[0..4], &[0, 0, 0, 255]); // bit=1 → black
        assert_eq!(&img.data[4..8], &[255, 255, 255, 255]); // bit=0 → white
    }

    #[test]
    fn cmyk8_to_rgba() {
        // Pure black in CMYK: C=0, M=0, Y=0, K=255
        let samples = vec![0, 0, 0, 255];
        let img = samples_cmyk8_to_rgba(&samples, 1, 1).unwrap();
        assert_eq!(&img.data[0..4], &[0, 0, 0, 255]);
    }
}
