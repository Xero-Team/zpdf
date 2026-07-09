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
        let page_id = self.page_id(page_index)?;
        let page_obj = self.resolve_current(page_id)?;
        let page_dict = page_obj.as_dict()?;

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
        let media_box = match page_dict.get("MediaBox") {
            Some(obj) => obj.clone(),
            None => self
                .find_inherited(page_id, "MediaBox")?
                .ok_or_else(|| invalid_data("page has no MediaBox"))?,
        };
        let bbox = media_box;

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

        let stamp_ref = self.add_flate_stream(&stamp_dict, &content_ops);
        let stamp_name = format!("ZPDFStamp{}", stamp_ref.0);

        // q/Q sandwich: prepend "q\n", append "\nQ q /ZPDFStampN Do Q\n".
        let q_stream_ref = self.add_stream(&PdfDict::new(), b"q\n");
        let tail_content = format!("\nQ q /{} Do Q\n", stamp_name);
        let tail_ref = self.add_stream(&PdfDict::new(), tail_content.as_bytes());

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

        // Merge /Resources: materialize inherited, deref if ref, merge /XObject.
        let res_obj = page_dict
            .get("Resources")
            .cloned()
            .or_else(|| self.find_inherited(page_id, "Resources").ok().flatten())
            .unwrap_or_else(|| PdfObject::Dict(PdfDict::new()));
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
        // Collision check: if /ZPDFStampN exists, increment N.
        let final_stamp_name = (0..1000)
            .map(|i| format!("ZPDFStamp{}", stamp_ref.0 + i))
            .find(|n| !xobj_dict.0.contains_key(&PdfName::new(n)))
            .unwrap_or(stamp_name);
        xobj_dict.insert(
            PdfName::new(&final_stamp_name),
            PdfObject::Ref(ObjectId(stamp_ref.0, stamp_ref.1 as u16)),
        );
        res_dict.insert(PdfName::new("XObject"), PdfObject::Dict(xobj_dict));
        new_page_dict.insert(PdfName::new("Resources"), PdfObject::Dict(res_dict));

        self.overwrite_object(page_id, PdfObject::Dict(new_page_dict));
        Ok(())
    }

    fn add_stamp_image(&mut self, image: &StampImage) -> Result<ObjectId> {
        match image {
            StampImage::Jpeg {
                data,
                width,
                height,
                components,
            } => {
                if *components == 4 {
                    return Err(unsupported("CMYK JPEG stamps are not supported yet").into());
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
                    PdfObject::Name(PdfName::new(match components {
                        1 => "DeviceGray",
                        3 => "DeviceRGB",
                        _ => return Err(invalid_data("invalid JPEG component count").into()),
                    })),
                );
                dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
                dict.insert(
                    PdfName::new("Filter"),
                    PdfObject::Name(PdfName::new("DCTDecode")),
                );
                let (num, gen) = self.add_stream(&dict, data);
                Ok(ObjectId(num, gen as u16))
            }
            StampImage::Rgb8 {
                width,
                height,
                pixels,
            } => {
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
                let (num, gen) = self.add_flate_stream(&dict, pixels);
                Ok(ObjectId(num, gen as u16))
            }
            StampImage::Rgba8 {
                width,
                height,
                pixels,
            } => {
                // RGB image with alpha as /SMask.
                let mut rgb = Vec::with_capacity((width * height * 3) as usize);
                let mut alpha = Vec::with_capacity((width * height) as usize);
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
                let (mask_num, mask_gen) = self.add_flate_stream(&mask_dict, &alpha);

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
                let (num, gen) = self.add_flate_stream(&dict, &rgb);
                Ok(ObjectId(num, gen as u16))
            }
        }
    }
}

/// Scan a JPEG byte stream for SOF0/1/2 markers to extract width, height, and
/// component count. Returns `None` when the scan fails (progressive, corrupted,
/// or other unsupported format).
pub fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32, u8)> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 2 > data.len() {
            return None;
        }
        let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if len < 2 || i + len > data.len() {
            return None;
        }
        if matches!(marker, 0xC0..=0xC2) {
            // SOF0/1/2
            if len < 8 {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 3], data[i + 4]]);
            let w = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            let c = data[i + 7];
            return Some((w as u32, h as u32, c));
        }
        i += len;
    }
    None
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
