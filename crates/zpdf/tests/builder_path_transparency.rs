//! End-to-end rendering tests for `DocumentBuilder::add_path_with_options`
//! (path fill/stroke opacity, blend modes, and graphics-state isolation).
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};
use zpdf_writer::{
    BlendMode, DocumentBuilder, ImageData, PathPaintOptions, PathSegment, PathStyle,
};

const SCALE: f32 = 1.0;

fn render_page(pdf: Vec<u8>, page_idx: usize) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(page_idx).expect("load page");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render")
}

fn px(page: &zpdf::cpu::RenderedPage, x: u32, pdf_y: u32) -> [u8; 3] {
    let y = page.height - 1 - pdf_y;
    let off = ((y * page.width + x) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

fn assert_rgb_near(actual: [u8; 3], expected: [u8; 3], tol: u8, label: &str) {
    for c in 0..3 {
        let diff = actual[c].abs_diff(expected[c]);
        assert!(
            diff <= tol,
            "{label}: channel {c} actual={actual:?} expected={expected:?} (diff={diff} > {tol})"
        );
    }
}

#[test]
fn fill_and_stroke_opacity_are_independent_across_zero_mid_and_one() {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(300.0, 120.0);

    // Three rectangles with fill=red (1,0,0) and thick blue stroke (0,0,1):
    // 1) fill_opacity=0.0, stroke_opacity=1.0 -> white interior, solid blue border
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 20.0,
                width: 60.0,
                height: 60.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.0, 0.0)),
                stroke: Some((0.0, 0.0, 1.0)),
                line_width: 12.0,
            },
            PathPaintOptions::new()
                .with_fill_opacity(0.0)
                .with_stroke_opacity(1.0),
        )
        .unwrap();

    // 2) fill_opacity=0.5, stroke_opacity=0.0 -> 50% red over white (~255,128,128), invisible border
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 120.0,
                y: 20.0,
                width: 60.0,
                height: 60.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.0, 0.0)),
                stroke: Some((0.0, 0.0, 1.0)),
                line_width: 12.0,
            },
            PathPaintOptions::new()
                .with_fill_opacity(0.5)
                .with_stroke_opacity(0.0),
        )
        .unwrap();

    // 3) fill_opacity=1.0, stroke_opacity=0.5 -> solid red interior, 50% blue stroke over white (~128,128,255) outside the fill
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 220.0,
                y: 20.0,
                width: 60.0,
                height: 60.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.0, 0.0)),
                stroke: Some((0.0, 0.0, 1.0)),
                line_width: 12.0,
            },
            PathPaintOptions::new()
                .with_fill_opacity(1.0)
                .with_stroke_opacity(0.5),
        )
        .unwrap();

    let rendered = render_page(builder.build().unwrap(), 0);

    // Rect 1: transparent fill, opaque blue stroke
    assert_rgb_near(px(&rendered, 50, 50), [255, 255, 255], 1, "rect1 interior");
    assert_rgb_near(px(&rendered, 20, 50), [0, 0, 255], 1, "rect1 border");

    // Rect 2: 50% red fill, transparent stroke (outer half of 12pt stroke is white)
    assert_rgb_near(px(&rendered, 150, 50), [255, 128, 128], 2, "rect2 interior");
    assert_rgb_near(
        px(&rendered, 117, 50),
        [255, 255, 255],
        1,
        "rect2 outer border",
    );

    // Rect 3: opaque red fill, 50% blue stroke (outer half over white = (128,128,255))
    assert_rgb_near(px(&rendered, 250, 50), [255, 0, 0], 1, "rect3 interior");
    assert_rgb_near(
        px(&rendered, 217, 50),
        [128, 128, 255],
        2,
        "rect3 outer border",
    );
}

#[test]
fn multiply_highlighter_over_colored_path_image_text_and_overlapping_strokes() {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(260.0, 220.0);

    // 1. Colored cyan backdrop path (0, 0.8, 1.0) at x=[20..120], y=[130..190].
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 130.0,
                width: 100.0,
                height: 60.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.8, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();

    // 2. Solid magenta RGB image (255, 0, 255) at x=[140..240], y=[130..190].
    builder
        .add_image(
            page,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels: vec![255, 0, 255, 255, 0, 255, 255, 0, 255, 255, 0, 255],
            },
            140.0,
            130.0,
            100.0,
            60.0,
        )
        .unwrap();

    // 3. Dark text row at y=80 (rendered via standard-14 Helvetica if system
    // font is available) PLUS an opaque black bar acting as guaranteed ink
    // under the highlight so pixel assertions are deterministic on fontless hosts.
    builder
        .add_text(
            page,
            "HIGHLIGHT",
            20.0,
            80.0,
            "Helvetica",
            20.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 55.0,
                width: 200.0,
                height: 15.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();

    // 4. First yellow Multiply highlighter stroke (1.0, 1.0, 0.0) with alpha 0.5
    // covering cyan path, magenta image, text/black bar, and white backdrop.
    let highlight = PathPaintOptions::new()
        .with_fill_opacity(0.5)
        .with_blend_mode(BlendMode::Multiply);
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 45.0,
                width: 240.0,
                height: 135.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            highlight,
        )
        .unwrap();

    // 5. Second overlapping yellow Multiply stroke over the right half of the
    // cyan path (x=[70..120]) and white area (x=[70..120], y=[105..125]).
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 70.0,
                y: 100.0,
                width: 55.0,
                height: 75.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            highlight,
        )
        .unwrap();

    let rendered = render_page(builder.build().unwrap(), 0);

    // Multiply(yellow=(1,1,0), alpha=0.5) over white (1,1,1):
    // C = (1 - 0.5)*1 + 0.5*(1*src) => (1.0, 1.0, 0.5) => [255, 255, 128]
    let single_on_white = px(&rendered, 30, 115);
    assert_rgb_near(
        single_on_white,
        [255, 255, 128],
        3,
        "single highlight on white",
    );

    // Second overlapping Multiply(yellow, alpha=0.5) over [1, 1, 0.5]:
    // blue = 0.5*0.5 + 0.5*(0.5*0) = 0.25 => [255, 255, 64]
    let double_on_white = px(&rendered, 95, 115);
    assert_rgb_near(
        double_on_white,
        [255, 255, 64],
        3,
        "double highlight on white",
    );
    assert!(
        double_on_white[2] < single_on_white[2],
        "overlapping Multiply strokes must darken the blue channel further"
    );

    // Single Multiply(yellow, 0.5) over cyan (0, 0.8, 1.0):
    // R: 0, G: 0.8, B: 0.5*1.0 + 0.5*(1.0*0) = 0.5 => [0, 204, 128]
    assert_rgb_near(
        px(&rendered, 40, 155),
        [0, 204, 128],
        3,
        "single highlight on cyan",
    );
    // Double Multiply(yellow, 0.5) over cyan: B = 0.25 => [0, 204, 64]
    assert_rgb_near(
        px(&rendered, 95, 155),
        [0, 204, 64],
        3,
        "double highlight on cyan",
    );

    // Multiply(yellow, 0.5) over magenta image (1, 0, 1):
    // R: 1, G: 0, B: 0.5 => [255, 0, 128]
    assert_rgb_near(
        px(&rendered, 190, 155),
        [255, 0, 128],
        3,
        "highlight over magenta image",
    );

    // Multiply(yellow, 0.5) over black ink (0, 0, 0): stays pure black [0, 0, 0]!
    // (Unlike Normal alpha blending, which would wash black text out to [128, 128, 0].)
    assert_rgb_near(
        px(&rendered, 100, 62),
        [0, 0, 0],
        1,
        "Multiply preserves black ink",
    );
}

#[test]
fn translucent_multiply_path_does_not_leak_to_subsequent_path_image_or_text() {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(260.0, 200.0);

    // Solid blue backdrop covering the whole page so any leaked opacity or
    // Multiply blend mode on later items will immediately change their pixels.
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 0.0,
                y: 0.0,
                width: 260.0,
                height: 200.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.0, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();

    // Translucent Multiply path first.
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 130.0,
                width: 80.0,
                height: 50.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            PathPaintOptions::new()
                .with_opacity(0.3)
                .with_blend_mode(BlendMode::Multiply),
        )
        .unwrap();

    // Subsequent normal path (opaque yellow): if opacity or Multiply leaked,
    // yellow over blue would turn dark/blue instead of pure [255, 255, 0].
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 20.0,
                width: 70.0,
                height: 70.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: Some((0.0, 1.0, 0.0)),
                line_width: 8.0,
            },
        )
        .unwrap();

    // Subsequent normal image (opaque red): if opacity or Multiply leaked,
    // red over blue would become black/purple instead of pure [255, 0, 0].
    builder
        .add_image(
            page,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels: vec![255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0],
            },
            100.0,
            20.0,
            70.0,
            70.0,
        )
        .unwrap();

    // Subsequent normal text (opaque white) — also verify the DisplayList command
    // emitted for the text run has alpha=1.0 and no active blend group.
    builder
        .add_text(
            page,
            "OPAQUE",
            180.0,
            45.0,
            "Helvetica",
            18.0,
            (1.0, 1.0, 1.0),
        )
        .unwrap();

    let pdf = builder.build().unwrap();
    let rendered = render_page(pdf.clone(), 0);

    // Subsequent path interior = pure opaque yellow, border = pure opaque green.
    assert_rgb_near(
        px(&rendered, 45, 55),
        [255, 255, 0],
        1,
        "subsequent path fill",
    );
    assert_rgb_near(
        px(&rendered, 10, 55),
        [0, 255, 0],
        1,
        "subsequent path stroke",
    );

    // Subsequent image = pure opaque red.
    assert_rgb_near(px(&rendered, 135, 55), [255, 0, 0], 1, "subsequent image");

    // Verify at DisplayList level that subsequent GlyphRun has alpha == 1.0 and
    // sits outside any PushBlendGroup.
    let doc = PdfDocument::open(pdf).unwrap();
    let p = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&p);
    let content = doc.page_content_bytes(&p).unwrap();
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(p.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &p.resources)
        .with_images(&mut images)
        .interpret(&content);
    let mut blend_depth = 0usize;
    let mut saw_text = false;
    for cmd in &dl.commands {
        match cmd {
            zpdf::display_list::RenderCommand::PushBlendGroup { .. } => blend_depth += 1,
            zpdf::display_list::RenderCommand::PopBlendGroup => blend_depth -= 1,
            zpdf::display_list::RenderCommand::DrawGlyphRun(run) => {
                saw_text = true;
                assert_eq!(blend_depth, 0, "text must not inherit Multiply blend group");
                assert!(
                    (run.alpha - 1.0).abs() < 1e-6,
                    "text must not inherit translucent alpha: {}",
                    run.alpha
                );
            }
            _ => {}
        }
    }
    assert!(saw_text, "expected subsequent text run in display list");
}

#[test]
fn all_standard_blend_modes_round_trip_through_pdf_names() {
    let modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::HardLight,
        BlendMode::SoftLight,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Hue,
        BlendMode::Saturation,
        BlendMode::Color,
        BlendMode::Luminosity,
    ];
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(100.0, 100.0);
    for mode in modes {
        builder
            .add_path_with_options(
                page,
                vec![PathSegment::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                }],
                PathStyle {
                    fill: Some((0.5, 0.5, 0.5)),
                    stroke: None,
                    line_width: 1.0,
                },
                PathPaintOptions::new()
                    .with_fill_opacity(0.5)
                    .with_blend_mode(mode),
            )
            .unwrap();
    }
    let pdf = builder.build().unwrap();
    let doc = PdfDocument::open(pdf).unwrap();
    let p = doc.page(0).unwrap();
    assert_eq!(p.resources.ext_g_state.len(), modes.len());
    let mut fonts = doc.load_page_fonts(&p);
    let content = doc.page_content_bytes(&p).unwrap();
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(p.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &p.resources)
        .with_images(&mut images)
        .interpret(&content);

    let interpreted_modes: Vec<BlendMode> = dl
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            zpdf::display_list::RenderCommand::PushBlendGroup { blend_mode, .. } => {
                Some(*blend_mode)
            }
            _ => None,
        })
        .collect();
    // Normal does not push a blend group; all 15 non-Normal modes must match in order.
    assert_eq!(interpreted_modes, modes[1..]);
}

/// Cross-check the generated PDF with an independent renderer (Poppler
/// `pdftoppm`) when available on `PATH`, avoiding self-only round-trip bias.
#[test]
fn independent_poppler_renderer_matches_opacity_multiply_and_isolation() {
    let Ok(status) = std::process::Command::new("pdftoppm")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    else {
        return;
    };
    if !status.success() {
        return;
    }

    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(260.0, 220.0);
    // Cyan backdrop path + black ink bar + magenta image.
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 130.0,
                width: 100.0,
                height: 60.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.8, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();
    builder
        .add_image(
            page,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels: vec![255, 0, 255, 255, 0, 255, 255, 0, 255, 255, 0, 255],
            },
            140.0,
            130.0,
            100.0,
            60.0,
        )
        .unwrap();
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 55.0,
                width: 200.0,
                height: 15.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();
    // Overlapping yellow Multiply highlighters (alpha 0.5).
    let highlight = PathPaintOptions::new()
        .with_fill_opacity(0.5)
        .with_blend_mode(BlendMode::Multiply);
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 45.0,
                width: 240.0,
                height: 135.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            highlight,
        )
        .unwrap();
    builder
        .add_path_with_options(
            page,
            vec![PathSegment::Rect {
                x: 70.0,
                y: 100.0,
                width: 55.0,
                height: 75.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            highlight,
        )
        .unwrap();
    // Subsequent opaque red rect over the black bar area — must not inherit
    // Multiply or 0.5 opacity (y=[10..35], drawn over an opaque blue patch).
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 10.0,
                width: 80.0,
                height: 25.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.0, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();
    builder
        .add_path(
            page,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 12.0,
                width: 60.0,
                height: 20.0,
            }],
            PathStyle {
                fill: Some((1.0, 1.0, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
        )
        .unwrap();

    let pdf = builder.build().unwrap();
    let zpdf_img = render_page(pdf.clone(), 0);

    let tmp_pdf =
        std::env::temp_dir().join(format!("zpdf-issue196-poppler-{}.pdf", std::process::id()));
    std::fs::write(&tmp_pdf, &pdf).expect("write temp pdf");
    let out = std::process::Command::new("pdftoppm")
        .args(["-r", "72", "-f", "1", "-l", "1"])
        .arg(&tmp_pdf)
        .output()
        .expect("run pdftoppm");
    let _ = std::fs::remove_file(&tmp_pdf);
    assert!(
        out.status.success(),
        "pdftoppm failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Parse binary PPM (P6 <w> <h> 255\n<rgb>).
    let ppm = out.stdout;
    let mut i = 0usize;
    let mut header_tokens = Vec::new();
    while header_tokens.len() < 4 {
        while i < ppm.len() && ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < ppm.len() && ppm[i] == b'#' {
            while i < ppm.len() && ppm[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < ppm.len() && !ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        header_tokens.push(std::str::from_utf8(&ppm[start..i]).unwrap().to_string());
    }
    assert_eq!(header_tokens[0], "P6");
    let w: u32 = header_tokens[1].parse().unwrap();
    let h: u32 = header_tokens[2].parse().unwrap();
    assert_eq!(header_tokens[3], "255");
    // Exactly one whitespace byte separates maxval from raw RGB pixels.
    let rgb = &ppm[i + 1..];
    assert_eq!(rgb.len(), (w * h * 3) as usize);
    assert_eq!((w, h), (zpdf_img.width, zpdf_img.height));

    let poppler_px = |x: u32, pdf_y: u32| -> [u8; 3] {
        let y = h - 1 - pdf_y;
        let off = ((y * w + x) * 3) as usize;
        [rgb[off], rgb[off + 1], rgb[off + 2]]
    };

    for (label, x, pdf_y) in [
        ("single highlight on white", 30, 115),
        ("double highlight on white", 95, 115),
        ("single highlight on cyan", 40, 155),
        ("double highlight on cyan", 95, 155),
        ("highlight over magenta image", 190, 155),
        ("Multiply preserves black ink", 100, 62),
        ("post-highlight opaque yellow over blue", 50, 22),
    ] {
        let actual_zpdf = px(&zpdf_img, x, pdf_y);
        let actual_poppler = poppler_px(x, pdf_y);
        assert_rgb_near(
            actual_zpdf,
            actual_poppler,
            3,
            &format!("zpdf vs poppler ({label})"),
        );
    }
}
