//! Regression tests for Type 3 glyph interpretation (#156):
//! - `d0` coloured glyphs retain their own colours (and do not leak to outer content)
//! - `d1` uncoloured glyphs ignore internal colour/shading operators and paint with the outer fill
//! - Resource lookup follows glyph → font → parent (Form XObjects, ExtGState, shadings, clips, nested fonts)
//! - Self-recursive Type 3 glyphs terminate safely via the recursion guard.
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend, TextSpan};

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

fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn render_and_extract(
    pdf: Vec<u8>,
) -> (
    zpdf::cpu::RenderedPage,
    Vec<TextSpan>,
    zpdf::InterpretStats,
    ImageCache,
) {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let mut spans = Vec::new();
    let interp = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_text_sink(&mut spans);
    let (dl, stats) = interp.interpret_with_stats(&content);
    let rendered = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    (rendered, spans, stats, images)
}

fn px(page: &zpdf::cpu::RenderedPage, x: u32, pdf_y: u32) -> [u8; 3] {
    let y = page.height - 1 - pdf_y;
    let off = ((y * page.width + x) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

#[test]
fn d0_glyph_keeps_own_colours_and_d1_locks_to_outer_fill() {
    // Glyph 'A' (65): d0 coloured glyph with red left half and blue right half.
    let glyph_d0 = b"1000 0 d0\n1 0 0 rg 0 0 500 1000 re f\n0 0 1 rg 500 0 500 1000 re f";
    // Glyph 'B' (66): d1 uncoloured glyph that tries to switch fill to red and
    // stroke to blue; both must be ignored and paint in the outer green fill.
    let glyph_d1 = b"1000 0 0 0 1000 1000 d1\n\
                     1 0 0 rg 0 0 400 1000 re f\n\
                     0 0 1 RG 200 w 750 100 m 750 900 l S";
    // Page sets dark green fill (0, 0.5, 0) and black stroke, draws "A", "B",
    // then draws an outer rectangle to ensure d0's blue fill did not leak.
    let content = b"0 0.5 0 rg 0 0 0 RG\n\
                    BT /F1 80 Tf 10 20 Td (A) Tj 100 0 Td (B) Tj ET\n\
                    210 20 30 80 re f";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 120] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
            .to_vec(),
        b"<< /gA 8 0 R /gB 9 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [65 /gA /gB] >>".to_vec(),
        stream_obj("", glyph_d0),
        stream_obj("", glyph_d1),
    ]);

    let (rendered, _, _, _) = render_and_extract(pdf);

    // d0 glyph 'A' at x=[10..90], y=[20..100]: red left, blue right.
    assert_eq!(
        px(&rendered, 30, 60),
        [255, 0, 0],
        "d0 left half must be red"
    );
    assert_eq!(
        px(&rendered, 70, 60),
        [0, 0, 255],
        "d0 right half must be blue"
    );

    // d1 glyph 'B' at x=[110..190], y=[20..100]: both fill and stroke must be outer green (0,128,0).
    assert_eq!(
        px(&rendered, 125, 60),
        [0, 128, 0],
        "d1 fill must ignore internal rg and use outer green"
    );
    assert_eq!(
        px(&rendered, 170, 60),
        [0, 128, 0],
        "d1 stroke must ignore internal RG and use outer green"
    );

    // Outer rectangle after Type 3 run must remain outer green (no leak from d0).
    assert_eq!(
        px(&rendered, 225, 60),
        [0, 128, 0],
        "outer fill colour must survive d0/d1 glyphs"
    );
}

#[test]
fn type3_glyph_resolves_form_xobjects_in_glyph_then_font_then_parent_order() {
    // Three Form XObjects:
    // - /FmShared on PAGE = red
    // - /FmShared on FONT = green (shadows page)
    // - /FmShared on GLYPH 'B' = blue (shadows font)
    // - /FmPageOnly on PAGE = yellow (inherited when absent in glyph and font)
    let form_red = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000]",
        b"1 0 0 rg 0 0 1000 1000 re f",
    );
    let form_green = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000]",
        b"0 1 0 rg 0 0 1000 1000 re f",
    );
    let form_blue = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000]",
        b"0 0 1 rg 0 0 1000 1000 re f",
    );
    let form_yellow = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 1000 1000]",
        b"1 1 0 rg 0 0 1000 1000 re f",
    );

    // Glyph 'A' (65): uses /FmShared from font resources -> green.
    let glyph_a = stream_obj("", b"1000 0 d0 /FmShared Do");
    // Glyph 'B' (66): carries its own /Resources overriding /FmShared -> blue.
    let glyph_b = stream_obj(
        "/Resources << /XObject << /FmShared 12 0 R >> >>",
        b"1000 0 d0 /FmShared Do",
    );
    // Glyph 'C' (67): references /FmPageOnly from parent page resources -> yellow.
    let glyph_c = stream_obj("", b"1000 0 d0 /FmPageOnly Do");

    let content = b"BT /F1 60 Tf 10 20 Td (A) Tj 80 0 Td (B) Tj 80 0 Td (C) Tj ET";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 100] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> \
                        /XObject << /FmShared 10 0 R /FmPageOnly 13 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", content),                       // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] \
          /Resources << /XObject << /FmShared 11 0 R >> >> >>"
            .to_vec(), // 5
        b"<< /gA 8 0 R /gB 9 0 R /gC 14 0 R >>".to_vec(), // 6
        b"<< /Type /Encoding /Differences [65 /gA /gB /gC] >>".to_vec(), // 7
        glyph_a,                                       // 8
        glyph_b,                                       // 9
        form_red,                                      // 10
        form_green,                                    // 11
        form_blue,                                     // 12
        form_yellow,                                   // 13
        glyph_c,                                       // 14
    ]);

    let (rendered, _, _, _) = render_and_extract(pdf);
    assert_eq!(
        px(&rendered, 40, 50),
        [0, 255, 0],
        "font /FmShared must shadow page"
    );
    assert_eq!(
        px(&rendered, 120, 50),
        [0, 0, 255],
        "glyph /FmShared must shadow font"
    );
    assert_eq!(
        px(&rendered, 200, 50),
        [255, 255, 0],
        "page /FmPageOnly must be inherited"
    );
}

#[test]
fn type3_d0_glyph_supports_ext_gstate_clip_shading_and_nested_font_without_polluting_text() {
    // Glyph 'A' (65): clips to left half [0..500], applies 50% ExtGState /GS1,
    // paints axial red->blue shading /Sh1, and draws nested Type 3 font /FSub
    // (which fills a small cyan square at [100..400] x [100..400]).
    let glyph_a = stream_obj(
        "",
        b"1000 0 d0\n\
          q 0 0 500 1000 re W n /GS1 gs /Sh1 sh Q\n\
          BT /FSub 1000 Tf 0 0 Td (X) Tj ET",
    );
    let glyph_sub = stream_obj("", b"1000 0 d0 0 1 1 rg 600 200 300 600 re f");
    let content = b"BT /F1 100 Tf 10 10 Td (A) Tj ET";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 140 120] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", content),                       // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 65 /Widths [1000] \
          /Resources << /ExtGState << /GS1 9 0 R >> /Shading << /Sh1 10 0 R >> \
                        /Font << /FSub 12 0 R >> >> >>"
            .to_vec(), // 5
        b"<< /A 8 0 R >>".to_vec(),                    // 6
        b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
        glyph_a,                                       // 8
        b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),    // 9
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 500 0] \
          /Function 11 0 R /Extend [true true] >>"
            .to_vec(), // 10
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(), // 11
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 13 0 R /Encoding 14 0 R \
          /FirstChar 88 /LastChar 88 /Widths [1000] /Resources << >> >>"
            .to_vec(), // 12
        b"<< /X 15 0 R >>".to_vec(),                   // 13
        b"<< /Type /Encoding /Differences [88 /X] >>".to_vec(), // 14
        glyph_sub,                                     // 15
    ]);

    let (rendered, spans, _, _) = render_and_extract(pdf);

    // Left side of clipped shading (x=15 -> near C0=red at ca=0.5 over white -> ~[255, 128, 128]).
    let left_sh = px(&rendered, 15, 60);
    assert!(
        left_sh[0] > 240 && (110..=150).contains(&left_sh[1]) && (110..=155).contains(&left_sh[2]),
        "clipped translucent shading left edge unexpected: {left_sh:?}"
    );
    // Right of clip [0..500] (x=65 -> outside clip, between shading and nested glyph): white.
    assert_eq!(
        px(&rendered, 65, 60),
        [255, 255, 255],
        "glyph clip must bound shading"
    );
    // Nested /FSub glyph at [600..900] -> page x=[70..100], y=[30..90]: cyan [0, 255, 255].
    assert_eq!(
        px(&rendered, 85, 60),
        [0, 255, 255],
        "nested font from font resources must render"
    );

    // Extracted text must contain only the outer glyph 'A', never the nested 'X'.
    let extracted: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(extracted, "A");
}

#[test]
fn self_recursive_type3_glyph_terminates_safely() {
    // Glyph 'A' tries to draw itself through the parent page's /F1 resource.
    let glyph_self = stream_obj("", b"1000 0 d0 BT /F1 1000 Tf (A) Tj ET");
    let content = b"BT /F1 50 Tf 10 10 Td (A) Tj ET";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
            .to_vec(),
        b"<< /A 8 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(),
        glyph_self,
    ]);

    let (_rendered, _spans, stats, _) = render_and_extract(pdf);
    assert!(
        stats.diagnostics.malformed_content > 0,
        "self-recursive Type 3 glyph must be caught and flagged as malformed_content"
    );
}

#[test]
fn type3_inline_image_mask_deduplicates_per_colour_and_supports_base_encoding() {
    // Glyph 'A' (65 via /BaseEncoding /WinAnsiEncoding): d1 uncoloured glyph
    // painting a solid 1x1 inline /ImageMask stencil scaled to [0..800]x[0..800].
    // Bit 0 in a default [0 1] ImageMask paints the current fill colour.
    let mut glyph_a_bytes = b"1000 0 0 0 1000 1000 d1\n\
        800 0 0 800 0 0 cm\n\
        BI /IM true /W 1 /H 1 /BPC 1 ID "
        .to_vec();
    glyph_a_bytes.push(0x00);
    glyph_a_bytes.extend_from_slice(b"\nEI");

    // Glyph 'B' (66 overridden via /Differences to /customB): green rectangle.
    let glyph_b_bytes = b"1000 0 d0 0 1 0 rg 0 0 800 800 re f";

    // Draw 'A' twice in red (must deduplicate into 1 cached image), once in
    // blue (must cache a second stencil image in blue), and 'B' via /Differences.
    let content = b"BT /F1 40 Tf\n\
                    1 0 0 rg 10 20 Td (AA) Tj\n\
                    0 0 1 rg 90 0 Td (A) Tj\n\
                    50 0 Td (B) Tj\n\
                    ET";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 220 80] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
            .to_vec(),
        b"<< /A 8 0 R /customB 9 0 R >>".to_vec(),
        b"<< /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [66 /customB] >>".to_vec(),
        stream_obj("", &glyph_a_bytes),
        stream_obj("", glyph_b_bytes),
    ]);

    let (rendered, _, _, images) = render_and_extract(pdf);
    assert_eq!(
        images.len(),
        2,
        "two red 'A' stencils must share one ImageCache entry and blue 'A' must get a second"
    );
    assert_eq!(px(&rendered, 25, 35), [255, 0, 0], "first red 'A'");
    assert_eq!(px(&rendered, 65, 35), [255, 0, 0], "second red 'A'");
    assert_eq!(px(&rendered, 115, 35), [0, 0, 255], "third blue 'A'");
    assert_eq!(
        px(&rendered, 165, 35),
        [0, 255, 0],
        "'B' via /Differences over /BaseEncoding"
    );
}
