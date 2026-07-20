//! Round-trip tests: PDFs authored by DocumentBuilder must parse and render
//! with zpdf's own reader.

use zpdf_document::PdfDocument;
use zpdf_writer::{DocumentBuilder, ImageData, PathSegment, PathStyle};

#[test]
fn built_pdf_with_path_round_trips() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_path(
            p,
            vec![
                PathSegment::Rect {
                    x: 10.0,
                    y: 10.0,
                    width: 100.0,
                    height: 50.0,
                },
                PathSegment::MoveTo { x: 20.0, y: 100.0 },
                PathSegment::LineTo { x: 150.0, y: 150.0 },
            ],
            PathStyle {
                stroke: Some((0.0, 0.0, 0.0)),
                fill: Some((1.0, 0.0, 0.0)),
                line_width: 2.0,
            },
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("re"), "rect op missing: {text}");
    assert!(text.contains("B"), "fill+stroke op missing: {text}");
}

#[test]
fn nan_path_is_rejected() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    let result = builder.add_path(
        p,
        vec![PathSegment::MoveTo {
            x: f64::NAN,
            y: 0.0,
        }],
        PathStyle::default(),
    );
    assert!(result.is_err());
}

#[test]
fn garbage_font_is_rejected() {
    let mut builder = DocumentBuilder::new();
    assert!(builder.embed_font(vec![0u8; 64]).is_err());
}

#[test]
fn built_pdf_opens_and_has_pages() {
    let mut builder = DocumentBuilder::new();
    let p1 = builder.add_page(612.0, 792.0);
    builder
        .add_text(
            p1,
            "Hello from zpdf!",
            72.0,
            720.0,
            "Helvetica",
            24.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder
        .add_text(
            p1,
            "Second line",
            72.0,
            680.0,
            "Times-Roman",
            14.0,
            (0.8, 0.1, 0.1),
        )
        .unwrap();
    builder.add_page(400.0, 600.0);

    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    assert_eq!(doc.page_count(), 2);

    let page = doc.page(0).unwrap();
    let rect = page.effective_box();
    assert_eq!((rect.x1 - rect.x0) as u32, 612);
    assert_eq!((rect.y1 - rect.y0) as u32, 792);
}

#[test]
fn built_pdf_content_stream_decodes() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(300.0, 300.0);
    builder
        .add_text(
            p,
            "content (with) \\ escapes",
            10.0,
            200.0,
            "Courier",
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let bytes = builder.build().unwrap();

    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(
        text.contains("BT"),
        "content must contain text block: {text}"
    );
    assert!(text.contains("Tj"), "content must show text: {text}");
    assert!(
        text.contains("content \\(with\\) \\\\ escapes"),
        "string must be escaped: {text}"
    );
}

#[test]
fn built_pdf_with_rgb_image_round_trips() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    // 2x2 red/green/blue/white image.
    let pixels = vec![
        255, 0, 0, 0, 255, 0, //
        0, 0, 255, 255, 255, 255,
    ];
    builder
        .add_image(
            p,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels,
            },
            50.0,
            50.0,
            100.0,
            100.0,
        )
        .unwrap();
    let bytes = builder.build().unwrap();

    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("/Im1 Do"), "image draw op missing: {text}");
}

#[test]
fn built_pdf_with_rgba_image_has_smask() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    let pixels = vec![
        255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 0, 255, 255, 255, 64,
    ];
    builder
        .add_image(
            p,
            ImageData::Rgba8 {
                width: 2,
                height: 2,
                pixels,
            },
            10.0,
            10.0,
            80.0,
            80.0,
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("/SMask"), "RGBA image must carry an SMask");

    // The document must still parse.
    let doc = PdfDocument::open(bytes.clone()).unwrap();
    assert_eq!(doc.page_count(), 1);
}

#[test]
fn image_buffer_size_mismatch_is_rejected() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_image(
            p,
            ImageData::Rgb8 {
                width: 10,
                height: 10,
                pixels: vec![0; 5], // wrong size
            },
            0.0,
            0.0,
            10.0,
            10.0,
        )
        .unwrap();
    assert!(builder.build().is_err());
}
