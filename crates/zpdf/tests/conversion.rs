//! End-to-end coverage for the high-level text/rich conversion API.

use zpdf::{convert_pdf, ConversionMode, ConversionOptions, PdfDocument};

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn conversion_fixture() -> Vec<u8> {
    let content = b"q 20 0 0 20 10 10 cm /Good Do Q \
        q 20 0 0 20 40 10 cm /Bad Do Q \
        BT /F1 12 Tf 20 100 Td (Text survives images) Tj ET";
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
          /Resources << /Font << /F1 4 0 R >> \
          /XObject << /Good 6 0 R /Bad 7 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        stream("", content),
        stream(
            "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
             /ColorSpace /DeviceRGB /BitsPerComponent 8",
            &[255, 0, 0],
        ),
        // Invalid dimensions: rich conversion must skip this image while keeping text.
        stream(
            "/Type /XObject /Subtype /Image /Width 0 /Height 1 \
             /ColorSpace /DeviceRGB /BitsPerComponent 8",
            &[0, 0, 0],
        ),
        b"<< /Title (Conversion Fixture) /Author (zpdf tests) >>".to_vec(),
    ];

    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 8 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[test]
fn text_only_conversion_extracts_no_rich_content() {
    let doc = PdfDocument::open(conversion_fixture()).expect("open fixture");
    let converted = convert_pdf(&doc, &[0], ConversionOptions::default()).expect("convert");

    assert_eq!(converted.pages[0].text, "Text survives images");
    assert!(converted.pages[0].images.is_empty());
    assert!(converted.info.is_none());
    assert!(converted.xmp.is_none());
}

#[test]
fn rich_conversion_keeps_text_and_skips_bad_images() {
    let doc = PdfDocument::open(conversion_fixture()).expect("open fixture");
    let converted = convert_pdf(
        &doc,
        &[0],
        ConversionOptions {
            mode: ConversionMode::Rich,
            use_structure: false,
        },
    )
    .expect("convert");

    let page = &converted.pages[0];
    assert_eq!(page.text, "Text survives images");
    assert_eq!(page.images.len(), 1, "the invalid image must be discarded");
    assert_eq!(
        (page.images[0].image.width, page.images[0].image.height),
        (1, 1)
    );
    assert_eq!(page.images[0].image.data, [255, 0, 0, 255]);
    assert_eq!(page.images[0].placements.len(), 1);
    assert_eq!(
        converted
            .info
            .as_ref()
            .and_then(|info| info.title.as_deref()),
        Some("Conversion Fixture")
    );
}

#[test]
fn rich_image_budget_exhaustion_does_not_drop_text() {
    let limits = zpdf::ParseLimits {
        max_image_cache_bytes: 0,
        ..zpdf::ParseLimits::default()
    };
    let doc = PdfDocument::open_with_limits(conversion_fixture(), limits).expect("open fixture");
    let converted = convert_pdf(
        &doc,
        &[0],
        ConversionOptions {
            mode: ConversionMode::Rich,
            use_structure: false,
        },
    )
    .expect("convert");

    assert_eq!(converted.pages[0].text, "Text survives images");
    assert!(converted.pages[0].images.is_empty());
}
