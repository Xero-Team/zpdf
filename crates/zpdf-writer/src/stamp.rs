//! Content stamping: overlay text and images onto existing pages.
//!
//! [`IncrementalWriter::stamp_page`] wraps all stamp content in one Form
//! XObject with self-contained `/Resources`, then appends it to the page's
//! `/Contents` via a q/Q sandwich (neutralizes unbalanced original streams).

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::{escape_text, standard_font_dict};

use crate::{invalid_data, unsupported, IncrementalWriter};

/// A single stamped item: text or image.
#[derive(Debug, Clone)]
pub enum StampItem {
    Text {
        text: String,
        x: f64,
        y: f64,
        /// Standard-14 font name (e.g., "Helvetica", "Times-Roman").
        font: String,
        size: f64,
        /// DeviceRGB color, each component in [0, 1].
        color: (f64, f64, f64),
    },
    Image {
        image: StampImage,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}

/// Image data for stamping.
#[derive(Debug, Clone)]
pub enum StampImage {
    /// JPEG data passed through with `/DCTDecode` (components: 1=Gray, 3=RGB).
    Jpeg {
        data: Vec<u8>,
        width: u32,
        height: u32,
        components: u8,
    },
    /// Raw RGB pixels, will be FlateDecode-compressed.
    Rgb8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// Raw RGBA pixels: RGB goes into the image, alpha into a `/SMask`.
    Rgba8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

impl IncrementalWriter {
    /// Stamp text and images onto a page. All items are wrapped in one Form
    /// XObject appended to the page's `/Contents` array via a q/Q sandwich.
    ///
    /// Coordinates are in PDF user space (bottom-left origin, unrotated).
    /// `/Rotate` is not compensated in this version.
    pub fn stamp_page(&mut self, page_index: usize, items: &[StampItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let required_objects = required_stamp_objects(items)?;
        self.ensure_object_capacity(required_objects)?;
        let page_id = self.page_id(page_index)?;
        let page_obj = self.resolve_current(page_id)?;
        let page_dict = page_obj.as_dict()?.clone();

        // Resolve inherited page data and choose the resource name before
        // adding streams. All document/input errors therefore leave the
        // pending update untouched.
        let bbox = match page_dict.get("MediaBox") {
            Some(obj) => obj.clone(),
            None => self
                .find_inherited(page_id, "MediaBox")?
                .ok_or_else(|| invalid_data("page has no MediaBox"))?,
        };
        let res_obj = match page_dict.get("Resources") {
            Some(obj) => obj.clone(),
            None => self
                .find_inherited(page_id, "Resources")?
                .unwrap_or_else(|| PdfObject::Dict(PdfDict::new())),
        };
        let mut res_dict = match self.deref_current(&res_obj) {
            PdfObject::Dict(d) => d,
            _ => PdfDict::new(),
        };
        let xobj_obj = res_dict
            .get("XObject")
            .cloned()
            .unwrap_or_else(|| PdfObject::Dict(PdfDict::new()));
        let mut xobj_dict = match self.deref_current(&xobj_obj) {
            PdfObject::Dict(d) => d,
            _ => PdfDict::new(),
        };
        let image_objects = required_objects - 3;
        let image_objects = u32::try_from(image_objects)
            .map_err(|_| invalid_data("too many stamp image objects"))?;
        let stamp_number = self
            .next_obj_num
            .checked_add(image_objects)
            .ok_or_else(|| invalid_data("object number space is exhausted"))?;
        let mut name_number = stamp_number;
        let final_stamp_name = loop {
            let candidate = format!("ZPDFStamp{name_number}");
            if !xobj_dict.0.contains_key(&PdfName::new(&candidate)) {
                break candidate;
            }
            name_number = name_number
                .checked_add(1)
                .ok_or_else(|| invalid_data("no free stamp resource name"))?;
        };

        // Build stamp resources: fonts + images.
        let mut fonts = PdfDict::new();
        let mut images = PdfDict::new();
        let mut font_counter = 0usize;
        let mut image_counter = 0usize;
        let mut content_ops = Vec::new();

        for item in items {
            match item {
                StampItem::Text {
                    text,
                    x,
                    y,
                    font,
                    size,
                    color,
                } => {
                    if !x.is_finite()
                        || !y.is_finite()
                        || !size.is_finite()
                        || *size <= 0.0
                        || !color.0.is_finite()
                        || !color.1.is_finite()
                        || !color.2.is_finite()
                        || !(0.0..=1.0).contains(&color.0)
                        || !(0.0..=1.0).contains(&color.1)
                        || !(0.0..=1.0).contains(&color.2)
                    {
                        return Err(invalid_data("invalid text stamp geometry or color").into());
                    }
                    let font_name = format!("ZF{}", font_counter);
                    font_counter += 1;
                    fonts.insert(
                        PdfName::new(&font_name),
                        PdfObject::Dict(standard_font_dict(font)),
                    );

                    push_str(&mut content_ops, "BT\n");
                    push_str(
                        &mut content_ops,
                        &format!("{} {} {} rg\n", color.0, color.1, color.2),
                    );
                    push_str(&mut content_ops, &format!("/{} {} Tf\n", font_name, size));
                    push_str(&mut content_ops, &format!("1 0 0 1 {} {} Tm\n", x, y));
                    content_ops.push(b'(');
                    escape_text(text, &mut content_ops);
                    push_str(&mut content_ops, ") Tj\nET\n");
                }
                StampItem::Image {
                    image,
                    x,
                    y,
                    width,
                    height,
                } => {
                    if !x.is_finite()
                        || !y.is_finite()
                        || !width.is_finite()
                        || !height.is_finite()
                        || *width <= 0.0
                        || *height <= 0.0
                    {
                        return Err(invalid_data("invalid image stamp geometry").into());
                    }
                    let img_name = format!("ZI{}", image_counter);
                    image_counter += 1;
                    let img_ref = self.add_stamp_image(image)?;
                    images.insert(PdfName::new(&img_name), PdfObject::Ref(img_ref));

                    push_str(&mut content_ops, "q\n");
                    push_str(
                        &mut content_ops,
                        &format!("{} 0 0 {} {} {} cm\n", width, height, x, y),
                    );
                    push_str(&mut content_ops, &format!("/{} Do\n", img_name));
                    push_str(&mut content_ops, "Q\n");
                }
            }
        }

        // Stamp Form XObject.
        let mut stamp_dict = PdfDict::new();
        stamp_dict.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("XObject")),
        );
        stamp_dict.insert(
            PdfName::new("Subtype"),
            PdfObject::Name(PdfName::new("Form")),
        );
        stamp_dict.insert(PdfName::new("FormType"), PdfObject::Integer(1));
        stamp_dict.insert(PdfName::new("BBox"), bbox);

        let mut resources = PdfDict::new();
        if !fonts.0.is_empty() {
            resources.insert(PdfName::new("Font"), PdfObject::Dict(fonts));
        }
        if !images.0.is_empty() {
            resources.insert(PdfName::new("XObject"), PdfObject::Dict(images));
        }
        stamp_dict.insert(PdfName::new("Resources"), PdfObject::Dict(resources));

        let stamp_ref = self.try_add_flate_stream(&stamp_dict, &content_ops)?;
        debug_assert_eq!(stamp_ref.0, stamp_number);

        // Merge /Resources before writing the tail stream so the content uses
        // the exact collision-free name inserted into /XObject.
        xobj_dict.insert(
            PdfName::new(&final_stamp_name),
            PdfObject::Ref(ObjectId(stamp_ref.0, stamp_ref.1 as u16)),
        );
        res_dict.insert(PdfName::new("XObject"), PdfObject::Dict(xobj_dict));

        // q/Q sandwich: prepend "q\n", append "\nQ q /ZPDFStampN Do Q\n".
        let q_stream_ref = self.try_add_stream(&PdfDict::new(), b"q\n")?;
        let tail_content = format!("\nQ q /{} Do Q\n", final_stamp_name);
        let tail_ref = self.try_add_stream(&PdfDict::new(), tail_content.as_bytes())?;

        // Rewrite page: /Contents → [q, ...originals, tail], /Resources merged.
        let mut new_page_dict = page_dict.clone();
        let orig_contents = match page_dict.get("Contents") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(r)) => vec![PdfObject::Ref(*r)],
            Some(PdfObject::Stream(_)) => {
                // Shouldn't happen (page dict streams are indirect), but handle it.
                vec![]
            }
            _ => vec![],
        };
        let mut new_contents = vec![PdfObject::Ref(ObjectId(
            q_stream_ref.0,
            q_stream_ref.1 as u16,
        ))];
        new_contents.extend(orig_contents);
        new_contents.push(PdfObject::Ref(ObjectId(tail_ref.0, tail_ref.1 as u16)));
        new_page_dict.insert(PdfName::new("Contents"), PdfObject::Array(new_contents));

        new_page_dict.insert(PdfName::new("Resources"), PdfObject::Dict(res_dict));

        self.overwrite_object(page_id, PdfObject::Dict(new_page_dict));
        Ok(())
    }

    fn add_stamp_image(&mut self, image: &StampImage) -> Result<ObjectId> {
        self.ensure_object_capacity(stamp_image_object_count(image)?)?;
        match image {
            StampImage::Jpeg {
                data,
                width,
                height,
                components,
            } => {
                let color_space = validate_jpeg_stamp(data, *width, *height, *components)?;
                let mut dict = PdfDict::new();
                dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("XObject")),
                );
                dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Image")),
                );
                dict.insert(PdfName::new("Width"), PdfObject::Integer(*width as i64));
                dict.insert(PdfName::new("Height"), PdfObject::Integer(*height as i64));
                dict.insert(
                    PdfName::new("ColorSpace"),
                    PdfObject::Name(PdfName::new(color_space)),
                );
                dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
                dict.insert(
                    PdfName::new("Filter"),
                    PdfObject::Name(PdfName::new("DCTDecode")),
                );
                let (num, gen) = self.try_add_stream(&dict, data)?;
                Ok(ObjectId(num, gen as u16))
            }
            StampImage::Rgb8 {
                width,
                height,
                pixels,
            } => {
                let expected = checked_image_len(*width, *height, 3)?;
                if pixels.len() != expected {
                    return Err(
                        invalid_data("RGB stamp buffer size does not match dimensions").into(),
                    );
                }
                let mut dict = PdfDict::new();
                dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("XObject")),
                );
                dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Image")),
                );
                dict.insert(PdfName::new("Width"), PdfObject::Integer(*width as i64));
                dict.insert(PdfName::new("Height"), PdfObject::Integer(*height as i64));
                dict.insert(
                    PdfName::new("ColorSpace"),
                    PdfObject::Name(PdfName::new("DeviceRGB")),
                );
                dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
                let (num, gen) = self.try_add_flate_stream(&dict, pixels)?;
                Ok(ObjectId(num, gen as u16))
            }
            StampImage::Rgba8 {
                width,
                height,
                pixels,
            } => {
                // RGB image with alpha as /SMask.
                let expected = checked_image_len(*width, *height, 4)?;
                if pixels.len() != expected {
                    return Err(
                        invalid_data("RGBA stamp buffer size does not match dimensions").into(),
                    );
                }
                let pixel_count = checked_image_len(*width, *height, 1)?;
                let rgb_len = pixel_count
                    .checked_mul(3)
                    .ok_or_else(|| invalid_data("RGB stamp dimensions overflow"))?;
                let mut rgb = Vec::with_capacity(rgb_len);
                let mut alpha = Vec::with_capacity(pixel_count);
                for chunk in pixels.chunks_exact(4) {
                    rgb.extend_from_slice(&chunk[..3]);
                    alpha.push(chunk[3]);
                }

                let mut mask_dict = PdfDict::new();
                mask_dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("XObject")),
                );
                mask_dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Image")),
                );
                mask_dict.insert(PdfName::new("Width"), PdfObject::Integer(*width as i64));
                mask_dict.insert(PdfName::new("Height"), PdfObject::Integer(*height as i64));
                mask_dict.insert(
                    PdfName::new("ColorSpace"),
                    PdfObject::Name(PdfName::new("DeviceGray")),
                );
                mask_dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
                let (mask_num, mask_gen) = self.try_add_flate_stream(&mask_dict, &alpha)?;

                let mut dict = PdfDict::new();
                dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("XObject")),
                );
                dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Image")),
                );
                dict.insert(PdfName::new("Width"), PdfObject::Integer(*width as i64));
                dict.insert(PdfName::new("Height"), PdfObject::Integer(*height as i64));
                dict.insert(
                    PdfName::new("ColorSpace"),
                    PdfObject::Name(PdfName::new("DeviceRGB")),
                );
                dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
                dict.insert(
                    PdfName::new("SMask"),
                    PdfObject::Ref(ObjectId(mask_num, mask_gen as u16)),
                );
                let (num, gen) = self.try_add_flate_stream(&dict, &rgb)?;
                Ok(ObjectId(num, gen as u16))
            }
        }
    }
}

const MAX_STAMP_IMAGE_PIXELS: u64 = 100_000_000;

fn required_stamp_objects(items: &[StampItem]) -> Result<usize> {
    // Form XObject + leading q stream + trailing invocation stream.
    let mut count = 3usize;
    for item in items {
        match item {
            StampItem::Text {
                x, y, size, color, ..
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !size.is_finite()
                    || *size <= 0.0
                    || !color.0.is_finite()
                    || !color.1.is_finite()
                    || !color.2.is_finite()
                    || !(0.0..=1.0).contains(&color.0)
                    || !(0.0..=1.0).contains(&color.1)
                    || !(0.0..=1.0).contains(&color.2)
                {
                    return Err(invalid_data("invalid text stamp geometry or color").into());
                }
            }
            StampItem::Image {
                image,
                x,
                y,
                width,
                height,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !width.is_finite()
                    || !height.is_finite()
                    || *width <= 0.0
                    || *height <= 0.0
                {
                    return Err(invalid_data("invalid image stamp geometry").into());
                }
                count = count
                    .checked_add(stamp_image_object_count(image)?)
                    .ok_or_else(|| invalid_data("too many stamp objects"))?;
            }
        }
    }
    Ok(count)
}

fn stamp_image_object_count(image: &StampImage) -> Result<usize> {
    match image {
        StampImage::Jpeg {
            data,
            width,
            height,
            components,
        } => {
            validate_jpeg_stamp(data, *width, *height, *components)?;
            Ok(1)
        }
        StampImage::Rgb8 {
            width,
            height,
            pixels,
        } => {
            let expected = checked_image_len(*width, *height, 3)?;
            if pixels.len() != expected {
                return Err(invalid_data("RGB stamp buffer size does not match dimensions").into());
            }
            Ok(1)
        }
        StampImage::Rgba8 {
            width,
            height,
            pixels,
        } => {
            let expected = checked_image_len(*width, *height, 4)?;
            if pixels.len() != expected {
                return Err(
                    invalid_data("RGBA stamp buffer size does not match dimensions").into(),
                );
            }
            Ok(2)
        }
    }
}

fn validate_jpeg_stamp(
    data: &[u8],
    width: u32,
    height: u32,
    components: u8,
) -> Result<&'static str> {
    let actual = jpeg_dimensions(data)
        .ok_or_else(|| invalid_data("invalid or unsupported JPEG stamp data"))?;
    let color_space = match actual.2 {
        1 => "DeviceGray",
        3 => "DeviceRGB",
        4 => return Err(unsupported("CMYK JPEG stamps are not supported yet").into()),
        _ => return Err(invalid_data("invalid JPEG component count").into()),
    };
    if components != 1 && components != 3 {
        return Err(invalid_data("invalid JPEG component count").into());
    }
    if actual != (width, height, components) {
        return Err(invalid_data("JPEG stamp metadata does not match its SOF header").into());
    }
    checked_image_len(width, height, usize::from(components))?;
    Ok(color_space)
}

fn checked_image_len(width: u32, height: u32, channels: usize) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(invalid_data("stamp image dimensions must be non-zero").into());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| invalid_data("stamp image dimensions overflow"))?;
    if pixels > MAX_STAMP_IMAGE_PIXELS {
        return Err(invalid_data("stamp image exceeds the 100000000-pixel limit").into());
    }
    let bytes = pixels
        .checked_mul(
            u64::try_from(channels)
                .map_err(|_| invalid_data("stamp image channel count is too large"))?,
        )
        .ok_or_else(|| invalid_data("stamp image dimensions overflow"))?;
    usize::try_from(bytes)
        .map_err(|_| invalid_data("stamp image exceeds addressable memory").into())
}

/// Scan an 8-bit JPEG byte stream for an SOF0/1/2 marker and extract its width,
/// height, and component count. Returns `None` for a missing SOI marker,
/// malformed segment structure, or an unsupported frame encoding.
pub fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32, u8)> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }

    let mut i = 2usize;
    while i < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        let marker = *data.get(i)?;
        i += 1;

        if marker == 0x00 || marker == 0xD8 || marker == 0xD9 || marker == 0xDA {
            return None;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }

        let len = usize::from(u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]));
        let end = i.checked_add(len)?;
        if len < 2 || end > data.len() {
            return None;
        }
        if matches!(marker, 0xC0..=0xC2) {
            let precision = *data.get(i + 2)?;
            let h = u16::from_be_bytes([*data.get(i + 3)?, *data.get(i + 4)?]);
            let w = u16::from_be_bytes([*data.get(i + 5)?, *data.get(i + 6)?]);
            let components = *data.get(i + 7)?;
            let expected_len = 8usize.checked_add(usize::from(components).checked_mul(3)?)?;
            if precision != 8 || w == 0 || h == 0 || components == 0 || len != expected_len {
                return None;
            }
            return Some((u32::from(w), u32::from(h), components));
        }
        // Other SOF encodings do not match the fixed 8-bit DCT image
        // dictionary emitted by the stamp writer.
        if matches!(
            marker,
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        ) {
            return None;
        }
        i = end;
    }
    None
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pdf_with_stamp_name_collision() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /ZPDFStamp5 4 0 R >> >> >>",
            "<< /Type /XObject /Subtype /Form /BBox [0 0 1 1] /Length 0 >>\nstream\n\nendstream",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn jpeg_with_sof(width: u16, height: u16, components: u8) -> Vec<u8> {
        let segment_len = 8u16 + u16::from(components) * 3;
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xC0];
        data.extend_from_slice(&segment_len.to_be_bytes());
        data.push(8);
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.push(components);
        for component in 0..components {
            data.extend_from_slice(&[component + 1, 0x11, 0]);
        }
        data.extend_from_slice(&[0xFF, 0xD9]);
        data
    }

    #[test]
    fn jpeg_sof0_parse() {
        // Minimal SOF0 marker: FF C0 <len:u16> 08 <height:u16> <width:u16> <components:u8> ...
        // Length must include itself (2 bytes) + all following fields
        let data = [
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, 0x00, 0x11, // SOF0, length=17
            0x08, // precision
            0x03, 0x00, // height = 768
            0x02, 0x00, // width = 512
            0x03, // components = 3
            // Need 3 * 3 = 9 more bytes for component info (id, sampling, qtable)
            0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xD9, // EOI
        ];
        assert_eq!(jpeg_dimensions(&data), Some((512, 768, 3)));
    }

    #[test]
    fn jpeg_no_sof() {
        let data = [0xFF, 0xD8, 0xFF, 0xD9];
        assert_eq!(jpeg_dimensions(&data), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xC0, 0, 8, 8, 0, 1, 0, 1, 0]), None);
    }

    #[test]
    fn image_length_validation_rejects_zero_and_overflow() {
        assert!(checked_image_len(0, 1, 4).is_err());
        assert!(checked_image_len(u32::MAX, u32::MAX, 4).is_err());
        assert!(checked_image_len(10_001, 10_000, 1).is_err());
    }

    #[test]
    fn jpeg_stamp_requires_matching_supported_sof_metadata() {
        let pdf = pdf_with_stamp_name_collision();
        let data = jpeg_with_sof(16, 8, 3);

        let mut valid_writer = IncrementalWriter::new(pdf.clone()).unwrap();
        let valid = StampImage::Jpeg {
            data: data.clone(),
            width: 16,
            height: 8,
            components: 3,
        };
        assert!(valid_writer.add_stamp_image(&valid).is_ok());
        assert_eq!(valid_writer.pending.len(), 1);

        let mut mismatch_writer = IncrementalWriter::new(pdf.clone()).unwrap();
        let mismatch = StampImage::Jpeg {
            data,
            width: 17,
            height: 8,
            components: 3,
        };
        assert!(mismatch_writer.add_stamp_image(&mismatch).is_err());
        assert!(mismatch_writer.pending.is_empty());

        let mut invalid_writer = IncrementalWriter::new(pdf.clone()).unwrap();
        let invalid = StampImage::Jpeg {
            data: Vec::new(),
            width: 1,
            height: 1,
            components: 3,
        };
        assert!(invalid_writer.add_stamp_image(&invalid).is_err());
        assert!(invalid_writer.pending.is_empty());

        let mut unsupported_writer = IncrementalWriter::new(pdf.clone()).unwrap();
        let unsupported = StampImage::Jpeg {
            data: jpeg_with_sof(1, 1, 4),
            width: 1,
            height: 1,
            components: 4,
        };
        assert!(unsupported_writer.add_stamp_image(&unsupported).is_err());
        assert!(unsupported_writer.pending.is_empty());

        let mut oversized_writer = IncrementalWriter::new(pdf).unwrap();
        let oversized = StampImage::Jpeg {
            data: jpeg_with_sof(u16::MAX, u16::MAX, 3),
            width: u32::from(u16::MAX),
            height: u32::from(u16::MAX),
            components: 3,
        };
        assert!(oversized_writer.add_stamp_image(&oversized).is_err());
        assert!(oversized_writer.pending.is_empty());
    }

    #[test]
    fn multi_object_stamps_preflight_exhaustion_without_mutating() {
        let pdf = pdf_with_stamp_name_collision();
        let mut writer = IncrementalWriter::new(pdf.clone()).unwrap();
        writer.next_obj_num = u32::MAX - 2;
        let result = writer.stamp_page(
            0,
            &[StampItem::Text {
                text: "test".to_string(),
                x: 10.0,
                y: 10.0,
                font: "Helvetica".to_string(),
                size: 12.0,
                color: (0.0, 0.0, 0.0),
            }],
        );
        assert!(result.is_err());
        assert!(writer.pending.is_empty());
        assert_eq!(writer.next_obj_num, u32::MAX - 2);

        let mut rgba_writer = IncrementalWriter::new(pdf).unwrap();
        rgba_writer.next_obj_num = u32::MAX - 1;
        let rgba = StampImage::Rgba8 {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        };
        assert!(rgba_writer.add_stamp_image(&rgba).is_err());
        assert!(rgba_writer.pending.is_empty());
        assert_eq!(rgba_writer.next_obj_num, u32::MAX - 1);
    }

    #[test]
    fn stamp_content_uses_collision_free_resource_name() {
        let mut writer = IncrementalWriter::new(pdf_with_stamp_name_collision()).unwrap();
        writer
            .stamp_page(
                0,
                &[StampItem::Text {
                    text: "test".to_string(),
                    x: 10.0,
                    y: 10.0,
                    font: "Helvetica".to_string(),
                    size: 12.0,
                    color: (0.0, 0.0, 0.0),
                }],
            )
            .unwrap();
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let text = String::from_utf8_lossy(output.get_ref());
        assert!(text.contains("/ZPDFStamp6 Do"));
        assert!(text.contains("/ZPDFStamp6 5 0 R"));
        assert!(!text.contains("/ZPDFStamp5 Do"));
    }

    #[test]
    fn stamp_rejects_non_finite_geometry_before_writing_content() {
        let mut writer = IncrementalWriter::new(pdf_with_stamp_name_collision()).unwrap();
        let result = writer.stamp_page(
            0,
            &[StampItem::Text {
                text: "test".to_string(),
                x: f64::NAN,
                y: 10.0,
                font: "Helvetica".to_string(),
                size: 12.0,
                color: (0.0, 0.0, 0.0),
            }],
        );
        assert!(result.is_err());
    }
}
