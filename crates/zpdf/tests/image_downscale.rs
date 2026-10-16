//! `ContentInterpreter::with_image_downscale` must not change CPU output: a
//! page rendered with images stored at their CPU footprint is byte-identical
//! to the same page rendered from the full decoded sources.
#![cfg(feature = "cpu-render")]

use std::io::Write;

use zpdf::cpu::{CpuRenderer, RenderedPage};
use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, ImageCache, ParseLimits, PdfDocument, RenderBackend};

/// Concatenate 1-based objects with a classic xref table + trailer.
fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", content.len()).into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

fn flate(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).expect("compress");
    encoder.finish().expect("compress")
}

/// Deterministic noise: no two neighbouring blocks average alike, so any
/// change in filtering shows up in the raster.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

fn image_obj(width: usize, height: usize, extra: &str, samples: &[u8]) -> Vec<u8> {
    stream_obj(
        &format!(
            "/Type /XObject /Subtype /Image /Width {width} /Height {height} {extra} /Filter /FlateDecode"
        ),
        &flate(samples),
    )
}

/// One 200x200pt page sharing every image kind under test, with `content`.
///
/// `/Im` 300x200 RGB, `/Al` the same with a 300x200 /SMask, `/St` a 300x200
/// /ImageMask stencil, `/Wide` 400x20 RGB, `/P0` a tiling pattern whose cell
/// draws `/Im`, `/GS0` a luminosity soft mask whose group draws `/Im`, and
/// `/Sh` an axial shading.
fn pdf(content: &[u8]) -> Vec<u8> {
    let rgb = noise(300 * 200 * 3, 1);
    let alpha = noise(300 * 200, 2);
    let stencil = noise(300_usize.div_ceil(8) * 200, 3);
    let wide = noise(400 * 20 * 3, 4);
    let cell: &[u8] = b"q 20 0 0 15 2 3 cm /Im Do Q";
    let group: &[u8] = b"q 120 0 0 90 30 40 cm /Im Do Q";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Im 5 0 R /Al 6 0 R /St 8 0 R /Wide 9 0 R >> \
          /Pattern << /P0 10 0 R >> /ExtGState << /GS0 11 0 R >> /Shading << /Sh 13 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        image_obj(300, 200, "/ColorSpace /DeviceRGB /BitsPerComponent 8", &rgb),
        image_obj(
            300,
            200,
            "/ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 7 0 R",
            &rgb,
        ),
        image_obj(300, 200, "/ColorSpace /DeviceGray /BitsPerComponent 8", &alpha),
        image_obj(300, 200, "/ImageMask true /BitsPerComponent 1", &stencil),
        image_obj(400, 20, "/ColorSpace /DeviceRGB /BitsPerComponent 8", &wide),
        stream_obj(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 25 25] \
             /XStep 25 /YStep 25 /Resources << /XObject << /Im 5 0 R >> >>",
            cell,
        ),
        b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 12 0 R >> >>".to_vec(),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceRGB >> /Resources << /XObject << /Im 5 0 R >> >>",
            group,
        ),
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 200 0] /Extend [true true] \
          /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>"
            .to_vec(),
    ])
}

/// Stored image sizes, in display-list order.
type Sizes = Vec<(u32, u32)>;

struct Rendered {
    page: RenderedPage,
    sizes: Sizes,
}

/// Which scale, if any, the interpreter stores image footprints for.
enum Footprints {
    Off,
    /// The scale the CPU renderer will rasterize at.
    Renderer,
    At(f32),
}

fn render(pdf: &[u8], scale: f32, limits: &ParseLimits, footprints: Footprints) -> Rendered {
    let doc = PdfDocument::open_with_limits(pdf.to_vec(), limits.clone()).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let page_box = page.effective_box();
    let mut interpreter = ContentInterpreter::new(page_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images);
    let footprint_scale = match footprints {
        Footprints::Off => None,
        Footprints::Renderer => Some(
            CpuRenderer::new()
                .with_limits(doc.file().limits())
                .raster_scale(page_box, scale)
                .expect("renderable page"),
        ),
        Footprints::At(scale) => Some(scale),
    };
    if let Some(footprint_scale) = footprint_scale {
        interpreter = interpreter.with_image_downscale(footprint_scale);
    }
    let dl = interpreter.interpret(&content);
    let mut sizes = Vec::new();
    let mut collect = |commands: &[RenderCommand]| {
        for command in commands {
            if let RenderCommand::DrawImage(draw) = command {
                let image = images.get(draw.image_id).expect("admitted image");
                sizes.push((image.width, image.height));
            }
        }
    };
    collect(&dl.commands);
    for command in &dl.commands {
        if let RenderCommand::PushBlendGroup {
            mask: Some(mask), ..
        } = command
        {
            collect(&mask.commands.commands);
        }
    }
    let page = CpuRenderer::new()
        .with_limits(doc.file().limits())
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, scale)
        .expect("cpu render");
    Rendered { page, sizes }
}

/// Render with and without footprint storage; the pixels must match exactly.
/// Returns the stored image sizes of both runs.
fn assert_identical(content: &[u8], scale: f32, limits: &ParseLimits) -> (Sizes, Sizes) {
    let pdf = pdf(content);
    let plain = render(&pdf, scale, limits, Footprints::Off);
    let fitted = render(&pdf, scale, limits, Footprints::Renderer);
    assert_eq!(
        (plain.page.width, plain.page.height),
        (fitted.page.width, fitted.page.height)
    );
    assert!(
        plain
            .page
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| *px != [255; 4]),
        "fixture must paint something"
    );
    if let Some(index) = plain
        .page
        .data
        .iter()
        .zip(&fitted.page.data)
        .position(|(a, b)| a != b)
    {
        let pixel = index / 4;
        panic!(
            "pixel ({}, {}) differs: {:?} without vs {:?} with footprint storage",
            pixel as u32 % plain.page.width,
            pixel as u32 / plain.page.width,
            &plain.page.data[pixel * 4..pixel * 4 + 4],
            &fitted.page.data[pixel * 4..pixel * 4 + 4],
        );
    }
    (plain.sizes, fitted.sizes)
}

fn largest(sizes: &[(u32, u32)]) -> u64 {
    sizes
        .iter()
        .map(|&(w, h)| w as u64 * h as u64)
        .max()
        .unwrap_or(0)
}

#[test]
fn single_minified_draw() {
    let (plain, fitted) = assert_identical(
        b"q 40 0 0 30 10 10 cm /Im Do Q",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(plain, [(300, 200)]);
    assert_eq!(
        fitted,
        [(41, 31)],
        "f32 footprints round up like the backend"
    );
}

#[test]
fn small_then_large_under_two_transforms() {
    let (plain, fitted) = assert_identical(
        b"q 40 0 0 30 10 10 cm /Im Do Q q 100 0 0 70 60 10 cm /Im Do Q \
          q 190 0 0 110 5 85 cm /Im Do Q",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(plain, [(300, 200); 3]);
    assert_eq!(fitted, [(41, 31), (100, 70), (300, 200)]);
}

#[test]
fn large_then_small_under_two_transforms() {
    let (_, fitted) = assert_identical(
        b"q 190 0 0 110 5 85 cm /Im Do Q q 100 0 0 70 60 10 cm /Im Do Q \
          q 40 0 0 30 10 10 cm /Im Do Q",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(fitted, [(300, 200), (100, 70), (41, 31)]);
}

#[test]
fn rotated_and_skewed_transforms() {
    let (_, fitted) = assert_identical(
        b"q 30 20 -15 25 100 50 cm /Im Do Q q 0 -45 35 0 20 180 cm /Im Do Q \
          q 60 10 30 40 40 100 cm /Al Do Q",
        2.0,
        &ParseLimits::default(),
    );
    assert!(
        largest(&fitted) < 300 * 200,
        "every draw is minified: {fitted:?}"
    );
}

#[test]
fn anisotropic_minification_keeps_the_magnified_axis() {
    let (_, fitted) = assert_identical(
        b"q 50 0 0 100 20 20 cm /Wide Do Q",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(fitted, [(50, 20)]);
}

#[test]
fn soft_masked_image() {
    let (_, fitted) = assert_identical(
        b"0 0 1 rg 0 0 200 200 re f q 45 0 0 35 20 20 cm /Al Do Q",
        1.5,
        &ParseLimits::default(),
    );
    assert_eq!(fitted.len(), 1);
    assert!(largest(&fitted) < 300 * 200);
}

#[test]
fn stencil_image_mask() {
    let (_, fitted) = assert_identical(
        b"1 0 0 rg q 40 0 0 30 10 10 cm /St Do Q 0 0 1 rg q 120 0 0 80 60 60 cm /St Do Q",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(fitted, [(41, 31), (120, 80)]);
}

#[test]
fn tiling_pattern_cell_image() {
    let (_, fitted) = assert_identical(
        b"/Pattern cs /P0 scn 10 10 180 180 re f",
        1.0,
        &ParseLimits::default(),
    );
    assert!(!fitted.is_empty());
    assert!(fitted.iter().all(|&size| size == (21, 16)), "{fitted:?}");
}

#[test]
fn soft_mask_group_image() {
    let (_, fitted) = assert_identical(
        b"1 0 0 rg 0 0 200 200 re f /GS0 gs 0 0 1 rg 0 0 200 200 re f",
        1.0,
        &ParseLimits::default(),
    );
    assert_eq!(fitted, [(120, 90)]);
}

#[test]
fn inline_image() {
    let samples = noise(120 * 80, 5);
    let hex: String = samples.iter().map(|byte| format!("{byte:02x}")).collect();
    let content =
        format!("q 30 0 0 20 10 150 cm BI /W 120 /H 80 /CS /G /BPC 8 /F /AHx ID {hex}> EI Q");
    let (plain, fitted) = assert_identical(content.as_bytes(), 1.0, &ParseLimits::default());
    assert_eq!(plain, [(120, 80)]);
    assert_eq!(fitted, [(30, 20)]);
}

#[test]
fn shading_raster() {
    let (plain, fitted) = assert_identical(
        b"q 10 10 180 180 re W n /Sh sh Q",
        0.25,
        &ParseLimits::default(),
    );
    assert!(
        largest(&fitted) < largest(&plain),
        "{plain:?} vs {fitted:?}"
    );
}

#[test]
fn page_pixel_budget_reduces_the_footprint_scale() {
    let limits = ParseLimits {
        max_page_pixels: 9_000,
        ..ParseLimits::default()
    };
    let scale = CpuRenderer::new()
        .with_limits(&limits)
        .raster_scale(zpdf::Rect::new(0.0, 0.0, 200.0, 200.0), 2.0)
        .expect("fits");
    assert!(scale < 0.5, "budget clamps the requested scale: {scale}");
    let (_, fitted) = assert_identical(
        b"q 150 0 0 100 10 10 cm /Im Do Q q 40 0 0 30 160 160 cm /St Do Q",
        2.0,
        &limits,
    );
    assert!(largest(&fitted) < 150 * 100, "{fitted:?}");
    let rendered = render(&pdf(b""), 2.0, &limits, Footprints::Off).page;
    assert_eq!(rendered.width, (200.0 * scale).ceil() as u32);
}

#[test]
fn footprints_for_another_scale_change_the_raster() {
    // Control for the checks above: the comparison does detect filtering
    // differences, so identity holds only at the renderer's own scale.
    let pdf = pdf(b"q 40 0 0 30 10 10 cm /Im Do Q");
    let limits = ParseLimits::default();
    let plain = render(&pdf, 1.0, &limits, Footprints::Off);
    let other = render(&pdf, 1.0, &limits, Footprints::At(0.7));
    assert_eq!(other.sizes, [(28, 21)]);
    assert_ne!(plain.page.data, other.page.data);
}
