use zpdf_writer::{DocumentBuilder, ImageData, PathSegment, PathStyle};

fn main() {
    let mut b = DocumentBuilder::new();
    let p = b.add_page(612.0, 792.0);
    b.add_text(
        p,
        "Hello from DocumentBuilder!",
        72.0,
        700.0,
        "Helvetica",
        28.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    b.add_text(
        p,
        "Red Times line",
        72.0,
        650.0,
        "Times-Roman",
        18.0,
        (0.9, 0.1, 0.1),
    )
    .unwrap();

    // Embedded font, when a system font is available.
    if let Ok(font) = std::fs::read("C:/Windows/Fonts/arial.ttf") {
        let fh = b.embed_font(font).unwrap();
        b.add_text_embedded(
            p,
            "Embedded Arial text",
            72.0,
            600.0,
            fh,
            16.0,
            (0.0, 0.3, 0.7),
        )
        .unwrap();
    }

    // Vector path: stroked+filled rounded-ish shape and a line.
    b.add_path(
        p,
        vec![PathSegment::Rect {
            x: 72.0,
            y: 480.0,
            width: 200.0,
            height: 80.0,
        }],
        PathStyle {
            stroke: Some((0.0, 0.0, 0.0)),
            fill: Some((1.0, 0.9, 0.2)),
            line_width: 2.0,
        },
    )
    .unwrap();
    b.add_path(
        p,
        vec![
            PathSegment::MoveTo { x: 300.0, y: 480.0 },
            PathSegment::CurveTo {
                x1: 350.0,
                y1: 560.0,
                x2: 420.0,
                y2: 480.0,
                x3: 470.0,
                y3: 560.0,
            },
        ],
        PathStyle {
            stroke: Some((0.2, 0.5, 0.2)),
            fill: None,
            line_width: 3.0,
        },
    )
    .unwrap();

    let px: Vec<u8> = (0..64 * 64)
        .flat_map(|i| {
            let v = ((i % 64) * 4) as u8;
            [v, 128, 255 - v]
        })
        .collect();
    b.add_image(
        p,
        ImageData::Rgb8 {
            width: 64,
            height: 64,
            pixels: px,
        },
        72.0,
        300.0,
        128.0,
        128.0,
    )
    .unwrap();

    std::fs::write("target/builder_demo.pdf", b.build().unwrap()).unwrap();
    println!("written");
}
