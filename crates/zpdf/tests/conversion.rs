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
    assert!(
        converted.is_complete(),
        "images are intentionally omitted in text mode"
    );
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
    assert_eq!(page.diagnostics.decode_failures, 1);
    assert!(converted.ensure_complete().is_err());
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
    assert!(converted.pages[0].diagnostics.resource_limit_reached);
}

fn assemble(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
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
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[test]
fn rich_conversion_extracts_images_from_tiling_patterns() {
    // 20×20 pattern cell painting a 1×1 green image scaled to 10×10 at (2, 3),
    // with pattern /Matrix translating by (10, 20), tiled across a 40×20 rect
    // (2 cells horizontally: i=0 and i=1).
    let cell = stream(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Matrix [1 0 0 1 10 20] \
         /Resources << /XObject << /Im1 6 0 R >> >>",
        b"q 10 0 0 10 2 3 cm /Im1 Do Q",
    );
    let img = stream(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
         /ColorSpace /DeviceRGB /BitsPerComponent 8",
        &[0, 200, 50],
    );
    let content = stream("", b"/Pattern cs /P0 scn 10 20 40 20 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
          /Resources << /Pattern << /P0 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        img,                                           // 6
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let text_only = convert_pdf(&doc, &[0], ConversionOptions::default()).expect("text mode");
    assert!(
        text_only.pages[0].images.is_empty(),
        "text-only mode must not decode tiling cell images"
    );

    let rich = convert_pdf(
        &doc,
        &[0],
        ConversionOptions {
            mode: ConversionMode::Rich,
            use_structure: false,
        },
    )
    .expect("rich mode");
    let page = &rich.pages[0];
    assert!(
        rich.is_complete(),
        "2-cell pattern should complete cleanly: {:?}",
        page.diagnostics
    );
    assert_eq!(
        page.images.len(),
        1,
        "pattern image XObject must be collected"
    );
    assert_eq!(page.images[0].image.data, [0, 200, 50, 255]);
    assert_eq!(
        page.images[0].placements.len(),
        2,
        "two-cell pattern must produce two transformed placements"
    );
    let mut xs: Vec<f64> = page.images[0]
        .placements
        .iter()
        .map(|p| {
            assert!((p.transform.a - 10.0).abs() < 1e-6);
            assert!((p.transform.d - 10.0).abs() < 1e-6);
            assert!((p.transform.f - 23.0).abs() < 1e-6);
            p.transform.e
        })
        .collect();
    xs.sort_by(|a, b| a.total_cmp(b));
    assert!((xs[0] - 12.0).abs() < 1e-6, "tile 0 x={}", xs[0]);
    assert!((xs[1] - 32.0).abs() < 1e-6, "tile 1 x={}", xs[1]);
}

#[test]
fn rich_conversion_bounds_dense_tiling_image_placements() {
    // 100×100 = 10,000 cells painting an image XObject: rich conversion must
    // collect the image while bounding placement expansion and signaling truncation.
    let cell = stream(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /XObject << /Im1 6 0 R >> >>",
        b"q 2 0 0 2 0 0 cm /Im1 Do Q",
    );
    let img = stream(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
         /ColorSpace /DeviceRGB /BitsPerComponent 8",
        &[10, 20, 240],
    );
    let content = stream("", b"/Pattern cs /P0 scn 0 0 200 200 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
          /Resources << /Pattern << /P0 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        content,
        cell,
        img,
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let rich = convert_pdf(
        &doc,
        &[0],
        ConversionOptions {
            mode: ConversionMode::Rich,
            use_structure: false,
        },
    )
    .expect("rich mode");
    let page = &rich.pages[0];
    assert_eq!(page.images.len(), 1);
    assert_eq!(page.images[0].image.data, [10, 20, 240, 255]);
    assert!(
        !page.images[0].placements.is_empty() && page.images[0].placements.len() <= 4096,
        "placements must stay bounded, got {}",
        page.images[0].placements.len()
    );
    assert!(
        page.diagnostics.resource_limit_reached,
        "truncating dense tiling placements must set resource_limit_reached"
    );
}
