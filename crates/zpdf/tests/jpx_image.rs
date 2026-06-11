//! End-to-end JPXDecode: a JPEG 2000 image XObject — with /ColorSpace and
//! /BitsPerComponent legally omitted (spec 7.4.9) — decodes through the
//! interpreter into a DrawImage, including a JPX-encoded /SMask.

use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument};

const W: u32 = 32;
const H: u32 = 24;
const RGB_JP2: &[u8] = include_bytes!("../../zpdf-image/tests/fixtures/rgb.jp2");
const RGB_REF: &[u8] = include_bytes!("../../zpdf-image/tests/fixtures/rgb_ref.raw");
const GRAY_J2K: &[u8] = include_bytes!("../../zpdf-image/tests/fixtures/gray.j2k");
const GRAY_REF: &[u8] = include_bytes!("../../zpdf-image/tests/fixtures/gray_ref.raw");

/// Assemble a one-page PDF from raw object bodies (object n is `objects[n-1]`)
/// with a correct xref table.
fn build_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut buf: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        buf.extend_from_slice(body);
        buf.extend_from_slice(b"\nendobj\n");
    }
    let xref_off = buf.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for off in &offsets {
        buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    buf
}

fn image_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let mut obj = format!(
        "<</Type/XObject/Subtype/Image/Width {W}/Height {H}/Filter/JPXDecode{dict_extra}/Length {}>>\nstream\n",
        data.len()
    )
    .into_bytes();
    obj.extend_from_slice(data);
    obj.extend_from_slice(b"\nendstream");
    obj
}

fn interpret_jpx_pdf(image_extra: &str, with_smask: bool) -> (Vec<RenderCommand>, ImageCache) {
    let mut objects = vec![
        b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
        b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
        b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Resources<</XObject<</Im0 5 0 R>>>>/Contents 4 0 R>>"
            .to_vec(),
        {
            let content = b"q 100 0 0 100 0 0 cm /Im0 Do Q";
            let mut obj = format!("<</Length {}>>\nstream\n", content.len()).into_bytes();
            obj.extend_from_slice(content);
            obj.extend_from_slice(b"\nendstream");
            obj
        },
        image_stream(image_extra, RGB_JP2),
    ];
    if with_smask {
        objects.push(image_stream("/ColorSpace/DeviceGray", GRAY_J2K));
    }

    let pdf = build_pdf(&objects);
    let doc = PdfDocument::open(pdf).expect("open");
    let page = doc.page(0).expect("page");
    let mut fonts = doc.load_page_fonts(&page);
    let bytes = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();

    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&bytes);
    (dl.commands, images)
}

fn drawn_image<'a>(
    commands: &[RenderCommand],
    images: &'a ImageCache,
) -> &'a zpdf::DecodedImage {
    let draw = commands
        .iter()
        .find_map(|c| match c {
            RenderCommand::DrawImage(d) => Some(d),
            _ => None,
        })
        .expect("expected a DrawImage command");
    images.get(draw.image_id).expect("image in cache")
}

#[test]
fn jpx_xobject_without_colorspace_decodes() {
    let (commands, images) = interpret_jpx_pdf("", false);
    let img = drawn_image(&commands, &images);
    assert_eq!((img.width, img.height), (W, H));
    for i in 0..(W * H) as usize {
        let want = &RGB_REF[i * 3..i * 3 + 3];
        assert_eq!(
            &img.data[i * 4..i * 4 + 4],
            &[want[0], want[1], want[2], 255],
            "pixel {i}"
        );
    }
}

#[test]
fn jpx_smask_stream_folds_as_alpha() {
    let (commands, images) = interpret_jpx_pdf("/SMask 6 0 R", true);
    let img = drawn_image(&commands, &images);
    assert_eq!((img.width, img.height), (W, H));
    assert!(img.has_alpha);
    assert!(img.premultiplied);
    let mul255 = |v: u8, a: u8| ((v as u32 * a as u32 + 127) / 255) as u8;
    for i in 0..(W * H) as usize {
        let a = GRAY_REF[i];
        let want = &RGB_REF[i * 3..i * 3 + 3];
        assert_eq!(
            &img.data[i * 4..i * 4 + 4],
            &[
                mul255(want[0], a),
                mul255(want[1], a),
                mul255(want[2], a),
                a
            ],
            "pixel {i}"
        );
    }
}
