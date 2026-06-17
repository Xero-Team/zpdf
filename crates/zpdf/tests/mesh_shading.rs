//! Mesh shading acceptance (CPU backend): a type-4 free-form Gouraud triangle
//! and a type-6 Coons patch painted with `sh` must show the expected Gouraud /
//! bilinear gradient inside the mesh and leave the background untouched outside
//! it. Exercises the full path: stream decode → mesh bit-stream decode →
//! tessellation → page-space transform → rasterize → `DrawImage` → CPU render.
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 2.0;
const PAGE: f64 = 200.0;

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

fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
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

fn px(page: &zpdf::cpu::RenderedPage, x: f64, y: f64) -> [u8; 3] {
    let ix = (x * SCALE as f64) as u32;
    let iy = ((PAGE - y) * SCALE as f64) as u32;
    let off = ((iy * page.width + ix) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

/// Encode a page-space coordinate (0..200) into an 8-bit raw value for the
/// `/Decode [0 200 …]` range used below.
fn coord(v: f64) -> u8 {
    (v / PAGE * 255.0).round().clamp(0.0, 255.0) as u8
}

const SHADING_DICT_TAIL: &str = "/ColorSpace /DeviceRGB /BitsPerCoordinate 8 \
    /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 200 0 200 0 1 0 1 0 1]";

fn page_with_shading(shading: Vec<u8>) -> Vec<u8> {
    let content = b"1 1 1 rg 0 0 200 200 re f\n/Sh1 sh".to_vec();
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Shading << /Sh1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", &content),
        shading,
    ])
}

#[test]
fn type4_free_form_gouraud_triangle() {
    // Triangle V0(40,40)=red, V1(160,40)=green, V2(40,160)=blue (3 flag-0 verts).
    let verts: [(f64, f64, [u8; 3]); 3] = [
        (40.0, 40.0, [255, 0, 0]),
        (160.0, 40.0, [0, 255, 0]),
        (40.0, 160.0, [0, 0, 255]),
    ];
    let mut data = Vec::new();
    for (x, y, rgb) in verts {
        data.push(0u8); // flag 0
        data.push(coord(x));
        data.push(coord(y));
        data.extend_from_slice(&rgb);
    }
    let sh = stream_obj(&format!("/ShadingType 4 {SHADING_DICT_TAIL}"), &data);
    let page = render(page_with_shading(sh));

    let near_v0 = px(&page, 48.0, 48.0);
    assert!(
        near_v0[0] > 150 && near_v0[0] > near_v0[1] && near_v0[0] > near_v0[2],
        "near red vertex should be red-dominant, got {near_v0:?}"
    );
    let near_v1 = px(&page, 150.0, 46.0);
    assert!(
        near_v1[1] > 150 && near_v1[1] > near_v1[0] && near_v1[1] > near_v1[2],
        "near green vertex should be green-dominant, got {near_v1:?}"
    );
    let near_v2 = px(&page, 46.0, 150.0);
    assert!(
        near_v2[2] > 150 && near_v2[2] > near_v2[0] && near_v2[2] > near_v2[1],
        "near blue vertex should be blue-dominant, got {near_v2:?}"
    );
    // Beyond the hypotenuse (x + y > 200): outside the triangle → white background.
    assert_eq!(
        px(&page, 165.0, 165.0),
        [255, 255, 255],
        "outside the triangle must stay white"
    );
}

#[test]
fn type4_mesh_as_shading_pattern_fill() {
    // Same triangle as `type4_free_form_gouraud_triangle`, but painted by filling
    // a rectangle with a PatternType-2 (shading) pattern via `/Pattern cs /P1 scn`
    // — exercises resolve_pattern's shading branch + PatternPaint::Shading, which
    // the `sh`-operator tests do not.
    let verts: [(f64, f64, [u8; 3]); 3] = [
        (40.0, 40.0, [255, 0, 0]),
        (160.0, 40.0, [0, 255, 0]),
        (40.0, 160.0, [0, 0, 255]),
    ];
    let mut data = Vec::new();
    for (x, y, rgb) in verts {
        data.push(0u8);
        data.push(coord(x));
        data.push(coord(y));
        data.extend_from_slice(&rgb);
    }
    let content = b"1 1 1 rg 0 0 200 200 re f\n/Pattern cs /P1 scn\n20 20 160 160 re f".to_vec();
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", &content),
        b"<< /PatternType 2 /Shading 6 0 R /Matrix [1 0 0 1 0 0] >>".to_vec(),
        stream_obj(&format!("/ShadingType 4 {SHADING_DICT_TAIL}"), &data),
    ]);
    let page = render(pdf);

    let near_v0 = px(&page, 48.0, 48.0);
    assert!(
        near_v0[0] > 150 && near_v0[0] > near_v0[1] && near_v0[0] > near_v0[2],
        "pattern fill near red vertex should be red-dominant, got {near_v0:?}"
    );
    let near_v2 = px(&page, 46.0, 150.0);
    assert!(
        near_v2[2] > 150 && near_v2[2] > near_v2[0] && near_v2[2] > near_v2[1],
        "pattern fill near blue vertex should be blue-dominant, got {near_v2:?}"
    );
    // Inside the fill rect but beyond the triangle hypotenuse → background.
    assert_eq!(
        px(&page, 150.0, 150.0),
        [255, 255, 255],
        "pattern paints only where the mesh covers"
    );
}

#[test]
fn type6_coons_patch_bilinear() {
    // Square patch [20,180]² with corner colours c1=red@p1(20,20),
    // c2=green@p4(20,180), c3=blue@p7(180,180), c4=white@p10(180,20).
    let t = |a: f64, b: f64, k: usize| a + (b - a) * (k as f64 / 3.0); // edge interpolation
    let mut pts: Vec<(f64, f64)> = Vec::new();
    // Left edge p1..p4: (20,20)→(20,180).
    for k in 0..4 {
        pts.push((20.0, t(20.0, 180.0, k)));
    }
    // Top edge p5..p7: (20,180)→(180,180) (skip the shared corner p4).
    for k in 1..4 {
        pts.push((t(20.0, 180.0, k), 180.0));
    }
    // Right edge p8..p10: (180,180)→(180,20).
    for k in 1..4 {
        pts.push((180.0, t(180.0, 20.0, k)));
    }
    // Bottom edge p11,p12: (180,20)→(20,20) interior controls.
    for k in 1..3 {
        pts.push((t(180.0, 20.0, k), 20.0));
    }
    assert_eq!(pts.len(), 12);

    let mut data = vec![0u8]; // flag 0
    for (x, y) in pts {
        data.push(coord(x));
        data.push(coord(y));
    }
    for rgb in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]] {
        data.extend_from_slice(&rgb);
    }
    let sh = stream_obj(&format!("/ShadingType 6 {SHADING_DICT_TAIL}"), &data);
    let page = render(page_with_shading(sh));

    // Patch center = bilinear average of the four corners ≈ mid-gray.
    let center = px(&page, 100.0, 100.0);
    let avg = (center[0] as i32 + center[1] as i32 + center[2] as i32) / 3;
    assert!(
        (100..=160).contains(&avg) && center.iter().all(|&c| (c as i32 - avg).abs() < 30),
        "patch center should be ~mid-gray, got {center:?}"
    );
    // Near the red corner p1=(20,20).
    let near_red = px(&page, 30.0, 30.0);
    assert!(
        near_red[0] > near_red[1] && near_red[0] > near_red[2] && near_red[0] > 120,
        "near p1 corner should be red-dominant, got {near_red:?}"
    );
    // Outside the patch (the patch spans only [20,180]²).
    assert_eq!(
        px(&page, 6.0, 6.0),
        [255, 255, 255],
        "outside the patch must stay white"
    );
}
