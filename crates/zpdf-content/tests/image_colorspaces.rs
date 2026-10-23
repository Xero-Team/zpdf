//! Image XObjects in special and CIE colour spaces resolve through the
//! interpreter's image colour-space path (indirect tint transforms, Lab
//! parameter dicts, stream palettes) instead of decaying to DeviceGray —
//! which inverted Separation tints and mis-sized multi-colorant DeviceN rows.

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::RenderCommand;
use zpdf_document::PdfDocument;
use zpdf_image::ImageCache;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

fn image(width: u32, colorspace: &str, data: &[u8]) -> Vec<u8> {
    stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width {width} /Height 1 \
             /BitsPerComponent 8 /ColorSpace {colorspace}"
        ),
        data,
    )
}

/// One page drawing images `/I1`..`/I6` in order, each in its own space.
fn build_pdf() -> Vec<u8> {
    let content: &[u8] = b"/I1 Do /I2 Do /I3 Do /I4 Do /I5 Do /I6 Do";
    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
              /Resources << /XObject << /I1 10 0 R /I2 11 0 R /I3 12 0 R \
              /I4 13 0 R /I5 14 0 R /I6 16 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, stream("", content)),
        // Tint transform t -> (0 0 0 t), indirect.
        (
            5,
            b"<< /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 0 0 1] /N 1 >>".to_vec(),
        ),
        // DeviceN (c m) -> (c m 0 0): a type 4 stream function.
        (
            6,
            stream(
                "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1]",
                b"{ 0 0 }",
            ),
        ),
        (
            7,
            b"<< /WhitePoint [0.9642 1 0.8249] /Range [-128 127 -128 127] >>".to_vec(),
        ),
        // Lab palette: L*=0 and L*=100, neutral a*/b*.
        (8, stream("", &[0, 128, 128, 255, 128, 128])),
        (9, b"[/Separation /Black /DeviceCMYK 5 0 R]".to_vec()),
        (10, image(2, "9 0 R", &[0, 255])),
        (
            11,
            image(
                3,
                "[/DeviceN [/Cyan /Magenta] /DeviceCMYK 6 0 R]",
                &[255, 0, 0, 255, 0, 0],
            ),
        ),
        (12, image(2, "[/Indexed [/Lab 7 0 R] 1 8 0 R]", &[0, 1])),
        (13, image(2, "[/Indexed 9 0 R 1 <00FF>]", &[0, 1])),
        (14, image(1, "[/Lab 7 0 R]", &[255, 128, 128])),
        // Half-transparent mask whose /Matte is palette entry 1 (black).
        (
            15,
            stream(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
                 /BitsPerComponent 8 /ColorSpace /DeviceGray /Matte [1]",
                &[128],
            ),
        ),
        // Indexed gray 128, pre-blended with that matte.
        (
            16,
            stream(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
                 /BitsPerComponent 8 /ColorSpace [/Indexed /DeviceRGB 1 <808080000000>] \
                 /SMask 15 0 R",
                &[0],
            ),
        ),
    ];

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in &objs {
        offsets.push(out.len());
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_off = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objs.len() + 1,
            xref_off
        )
        .as_bytes(),
    );
    out
}

fn rgb(cmyk: [f64; 4]) -> [u8; 3] {
    let (r, g, b) = zpdf_color::cmyk_to_rgb(cmyk[0], cmyk[1], cmyk[2], cmyk[3]);
    [r, g, b].map(|v| (v * 255.0).round() as u8)
}

/// Interpret the page and return each drawn image's RGB pixels, in order.
fn decode_images(
    output_intent: Option<std::sync::Arc<zpdf_color::IccTransform>>,
) -> Vec<Vec<[u8; 3]>> {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images);
    if let Some(oi) = output_intent {
        interp = interp.with_output_intent_cmyk(oi);
    }
    let list = interp.interpret(&content);
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawImage(draw) => images.get(draw.image_id),
            _ => None,
        })
        .map(|image| {
            image
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|px| [px[0], px[1], px[2]])
                .collect()
        })
        .collect()
}

#[test]
fn special_and_lab_image_colorspaces_resolve() {
    let decoded = decode_images(None);
    assert_eq!(decoded.len(), 6, "every image decodes");
    let px = |image: usize, i: usize| decoded[image][i];

    // Separation /Black: tint 0 is no ink, tint 1 is black ink.
    assert_eq!(px(0, 0), [255, 255, 255]);
    assert_eq!(px(0, 1), rgb([0.0, 0.0, 0.0, 1.0]));

    // Two-colorant DeviceN: two bytes per pixel, three pixels.
    assert_eq!(decoded[1].len(), 3);
    assert_eq!(px(1, 0), rgb([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(px(1, 1), rgb([0.0, 1.0, 0.0, 0.0]));
    assert_eq!(px(1, 2), [255, 255, 255]);

    // Indexed over Lab with a stream palette: black, then white.
    assert!(px(2, 0).iter().all(|&v| v < 8), "{:?}", px(2, 0));
    assert!(px(2, 1).iter().all(|&v| v > 247), "{:?}", px(2, 1));

    // Indexed over the Separation: palette entries are tints.
    assert_eq!(px(3, 0), [255, 255, 255]);
    assert_eq!(px(3, 1), rgb([0.0, 0.0, 0.0, 1.0]));

    // Lab with an indirect parameter dict.
    assert!(px(4, 0).iter().all(|&v| v > 247), "{:?}", px(4, 0));

    // An Indexed parent's /Matte is a palette index: un-blending gray 128
    // from black at alpha 1/2 gives white, premultiplied back to 128. Read
    // as a gray level, the same /Matte [1] would un-blend from white.
    assert!(
        px(5, 0).iter().all(|&v| v.abs_diff(128) <= 1),
        "{:?}",
        px(5, 0)
    );
}

#[test]
fn separation_cmyk_alternate_follows_the_output_intent() {
    // Like the `k` operator, a tint transform's DeviceCMYK output renders
    // through the document's CMYK output intent when one is active.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zpdf-color/src/testdata/cmyk_lut.icc"
    ))
    .expect("CMYK profile fixture");
    let oi = std::sync::Arc::new(
        zpdf_color::IccTransform::from_profile_bytes(&bytes, zpdf_color::RenderIntent::default())
            .expect("CMYK profile"),
    );
    let (r, g, b) = oi.color_to_rgb(&[0.0, 0.0, 0.0, 1.0]);
    let full_black = [r, g, b].map(|v| (v * 255.0).round() as u8);
    assert_ne!(
        full_black,
        rgb([0.0, 0.0, 0.0, 1.0]),
        "profile differs from SWOP"
    );

    let decoded = decode_images(Some(oi));
    assert_eq!(decoded[0][1], full_black, "Separation image");
    assert_eq!(decoded[3][1], full_black, "Indexed over the Separation");
}
