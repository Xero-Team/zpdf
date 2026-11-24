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

#[test]
fn type3_word_with_to_unicode_paints_once_and_preserves_semantic_run() {
    // Semi-transparent red d0 glyph (ca = 0.5) over white renders as
    // [255, 128, 128] when painted once, vs [255, 64, 64] if double-painted.
    let to_unicode = b"/CIDInit /ProcSet findresource begin\n\
        12 dict begin begincmap\n\
        1 begincodespacerange <01> <02> endcodespacerange\n\
        2 beginbfchar <01> <0048> <02> <0069> endbfchar\n\
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let glyph = stream_obj("", b"1000 0 d0 /GS1 gs 1 0 0 rg 0 0 1000 1000 re f");
    let content = b"BT /F1 60 Tf 10 20 Td <0102> Tj ET";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 160 100] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> /ExtGState << /GS1 9 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /ToUnicode 8 0 R /FirstChar 1 /LastChar 2 /Widths [1000 1000] \
          /Resources << >> >>"
            .to_vec(),
        b"<< /g1 10 0 R /g2 10 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [1 /g1 /g2] >>".to_vec(),
        stream_obj("", to_unicode),
        b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),
        glyph,
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let interp = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images);
    let dl = interp.interpret(&content);

    assert!(
        !dl.commands
            .iter()
            .any(|c| matches!(c, zpdf::display_list::RenderCommand::DrawGlyphRun(_))),
        "Type 3 glyphs must paint via expanded commands only, never a duplicate DrawGlyphRun"
    );
    let expanded = dl.expanded_glyph_runs();
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].run.glyphs.len(), 2);
    assert_eq!(
        (
            expanded[0].run.glyphs[0].code,
            expanded[0].run.glyphs[1].code
        ),
        (1, 2)
    );

    let rendered = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    let p = px(&rendered, 40, 50);
    assert!(
        p[0] == 255 && (126..=130).contains(&p[1]) && (126..=130).contains(&p[2]),
        "translucent Type 3 glyph must be painted once (~[255,128,128]), got {p:?}"
    );
}

#[test]
fn type3_inner_indirect_resources_shadow_outer_inline_resources() {
    // Page defines DIRECT (inline) /ExtGState /GS1 (ca=0.2), /ColorSpace /CS1
    // (green Indexed), and /Shading /Sh1 (green axial shading).
    // Each glyph defines an INDIRECT resource under the same name that must
    // shadow the page's direct entry within the glyph scope.
    let glyph_gs = stream_obj(
        "/Resources << /ExtGState << /GS1 11 0 R >> >>",
        b"1000 0 d0 /GS1 gs 1 0 0 rg 0 0 1000 1000 re f",
    );
    let glyph_cs = stream_obj(
        "/Resources << /ColorSpace << /CS1 12 0 R >> >>",
        b"1000 0 d0 /CS1 cs 0 sc 0 0 1000 1000 re f",
    );
    let glyph_sh = stream_obj(
        "/Resources << /Shading << /Sh1 13 0 R >> >>",
        b"1000 0 d0 0 0 1000 1000 re W n /Sh1 sh",
    );
    let content = b"BT /F1 60 Tf 10 20 Td (A) Tj 80 0 Td (B) Tj 80 0 Td (C) Tj ET";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 100] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> \
                        /ExtGState << /GS1 << /Type /ExtGState /ca 0.2 >> >> \
                        /ColorSpace << /CS1 [/Indexed /DeviceRGB 0 <00FF00>] >> \
                        /Shading << /Sh1 << /ShadingType 2 /ColorSpace /DeviceRGB \
                                            /Coords [0 0 1000 0] /Function 14 0 R \
                                            /Extend [true true] >> >> >> >>"
            .to_vec(), // 3
        stream_obj("", content),                       // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> >>"
            .to_vec(), // 5
        b"<< /gA 8 0 R /gB 9 0 R /gC 10 0 R >>".to_vec(), // 6
        b"<< /Type /Encoding /Differences [65 /gA /gB /gC] >>".to_vec(), // 7
        glyph_gs,                                      // 8
        glyph_cs,                                      // 9
        glyph_sh,                                      // 10
        b"<< /Type /ExtGState /ca 1.0 >>".to_vec(),    // 11 (indirect opaque GS)
        b"[/Indexed /DeviceRGB 0 <0000FF>]".to_vec(),  // 12 (indirect blue CS)
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 1000 0] \
          /Function 15 0 R /Extend [true true] >>"
            .to_vec(), // 13 (indirect red shading)
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0 1 0] /C1 [0 1 0] /N 1 >>".to_vec(), // 14 (green)
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [1 0 0] /N 1 >>".to_vec(), // 15 (red)
    ]);

    let (rendered, _, _, _) = render_and_extract(pdf);
    assert_eq!(
        px(&rendered, 40, 50),
        [255, 0, 0],
        "glyph indirect /GS1 (ca=1.0) must shadow page inline /GS1 (ca=0.2)"
    );
    assert_eq!(
        px(&rendered, 120, 50),
        [0, 0, 255],
        "glyph indirect /CS1 (blue) must shadow page inline /CS1 (green)"
    );
    assert_eq!(
        px(&rendered, 200, 50),
        [255, 0, 0],
        "glyph indirect /Sh1 (red) must shadow page inline /Sh1 (green)"
    );
}

#[test]
fn type3_three_scope_font_rebinding_back_to_page_font_wins() {
    // Page maps /F1 -> FontA (red), /FHost -> FontHost.
    // FontHost maps /F1 -> FontB (green).
    // - Glyph 'A' in FontHost has no /Resources and draws /F1 -> FontB (green).
    // - Glyph 'B' in FontHost explicitly maps /F1 -> FontA (same object ref as
    //   the page!) and draws /F1 -> MUST resolve to FontA (red), NOT FontB.
    let glyph_from_host = stream_obj("", b"1000 0 d0 BT /F1 1000 Tf (X) Tj ET");
    let glyph_rebound_to_page = stream_obj(
        "/Resources << /Font << /F1 10 0 R >> >>",
        b"1000 0 d0 BT /F1 1000 Tf (X) Tj ET",
    );
    let glyph_red = stream_obj("", b"1000 0 d0 1 0 0 rg 0 0 1000 1000 re f");
    let glyph_green = stream_obj("", b"1000 0 d0 0 1 0 rg 0 0 1000 1000 re f");
    let content = b"BT /FHost 60 Tf 10 20 Td (A) Tj 80 0 Td (B) Tj ET";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Contents 4 0 R \
          /Resources << /Font << /F1 10 0 R /FHost 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", content),                       // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 66 /Widths [1000 1000] \
          /Resources << /Font << /F1 14 0 R >> >> >>"
            .to_vec(), // 5 (FontHost maps /F1 -> FontB)
        b"<< /gA 8 0 R /gB 9 0 R >>".to_vec(),         // 6
        b"<< /Type /Encoding /Differences [65 /gA /gB] >>".to_vec(), // 7
        glyph_from_host,                               // 8
        glyph_rebound_to_page,                         // 9
        // FontA (10..13): red 'X'
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 11 0 R /Encoding 12 0 R \
          /FirstChar 88 /LastChar 88 /Widths [1000] /Resources << >> >>"
            .to_vec(), // 10
        b"<< /X 13 0 R >>".to_vec(),                            // 11
        b"<< /Type /Encoding /Differences [88 /X] >>".to_vec(), // 12
        glyph_red,                                              // 13
        // FontB (14..17): green 'X'
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 15 0 R /Encoding 16 0 R \
          /FirstChar 88 /LastChar 88 /Widths [1000] /Resources << >> >>"
            .to_vec(), // 14
        b"<< /X 17 0 R >>".to_vec(),                            // 15
        b"<< /Type /Encoding /Differences [88 /X] >>".to_vec(), // 16
        glyph_green,                                            // 17
    ]);

    let (rendered, _, _, _) = render_and_extract(pdf);
    assert_eq!(
        px(&rendered, 40, 50),
        [0, 255, 0],
        "glyph without own /Resources must inherit FontHost's /F1 (FontB = green)"
    );
    assert_eq!(
        px(&rendered, 120, 50),
        [255, 0, 0],
        "glyph explicitly mapping /F1 back to page's FontA must render red, not FontB"
    );
}

#[test]
fn type3_inline_image_cache_respects_distinct_inherited_colorspace_palettes() {
    // Glyph 'A' paints a 1x1 inline image whose /CS names `/Ink` (inherited
    // from enclosing Form XObjects). All placements draw at the exact same size.
    // - Form 1 (indirect red /Ink): paints 'A' twice -> both red, deduplicated.
    // - Form 2 (indirect blue /Ink): paints 'A' once -> must be blue, not cached red.
    // - Form 3 (inline green /Ink) & Form 4 (inline yellow /Ink): must paint
    //   green and yellow respectively without aliasing.
    let mut glyph_bytes = b"1000 0 d0\n1000 0 0 1000 0 0 cm\n\
        BI /W 1 /H 1 /BPC 8 /CS /Ink ID "
        .to_vec();
    glyph_bytes.push(0x00);
    glyph_bytes.extend_from_slice(b"\nEI");

    let form_1 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 120 80] \
         /Resources << /Font << /F1 5 0 R >> /ColorSpace << /Ink 13 0 R >> >>",
        b"BT /F1 40 Tf 10 20 Td (A) Tj 50 0 Td (A) Tj ET",
    );
    let form_2 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 80 80] \
         /Resources << /Font << /F1 5 0 R >> /ColorSpace << /Ink 14 0 R >> >>",
        b"BT /F1 40 Tf 10 20 Td (A) Tj ET",
    );
    let form_3 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 80 80] \
         /Resources << /Font << /F1 5 0 R >> \
                       /ColorSpace << /Ink [/Indexed /DeviceRGB 0 <00FF00>] >> >>",
        b"BT /F1 40 Tf 10 20 Td (A) Tj ET",
    );
    let form_4 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 80 80] \
         /Resources << /Font << /F1 5 0 R >> \
                       /ColorSpace << /Ink [/Indexed /DeviceRGB 0 <FFFF00>] >> >>",
        b"BT /F1 40 Tf 10 20 Td (A) Tj ET",
    );

    let content = b"q /Fm1 Do Q \
                    q 1 0 0 1 110 0 cm /Fm2 Do Q \
                    q 1 0 0 1 170 0 cm /Fm3 Do Q \
                    q 1 0 0 1 230 0 cm /Fm4 Do Q";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 80] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> \
                        /XObject << /Fm1 9 0 R /Fm2 10 0 R /Fm3 11 0 R /Fm4 12 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", content),                       // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
            .to_vec(), // 5
        b"<< /A 8 0 R >>".to_vec(),                    // 6
        b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(), // 7
        stream_obj("", &glyph_bytes),                  // 8
        form_1,                                        // 9
        form_2,                                        // 10
        form_3,                                        // 11
        form_4,                                        // 12
        b"[/Indexed /DeviceRGB 0 <FF0000>]".to_vec(),  // 13 (red)
        b"[/Indexed /DeviceRGB 0 <0000FF>]".to_vec(),  // 14 (blue)
    ]);

    let (rendered, _, _, images) = render_and_extract(pdf);
    // Form 1's two draws share 1 cached image; Form 2/3/4 each decode distinct palettes.
    assert_eq!(
        images.len(),
        4,
        "Form 1 repeats must deduplicate while distinct palettes decode separately"
    );
    assert_eq!(
        px(&rendered, 30, 40),
        [255, 0, 0],
        "Form 1 first placement (red)"
    );
    assert_eq!(
        px(&rendered, 80, 40),
        [255, 0, 0],
        "Form 1 second placement (red)"
    );
    assert_eq!(
        px(&rendered, 140, 40),
        [0, 0, 255],
        "Form 2 placement (blue)"
    );
    assert_eq!(
        px(&rendered, 200, 40),
        [0, 255, 0],
        "Form 3 inline palette (green)"
    );
    assert_eq!(
        px(&rendered, 260, 40),
        [255, 255, 0],
        "Form 4 inline palette (yellow)"
    );
}

#[test]
fn type3_glyph_strokes_and_thin_fills_are_isolated_from_table_rule_capture() {
    // Glyph 'A' draws both a long thin stroke (10 * 0.1 = 1pt wide, 100pt long)
    // and a long thin filled rect (1pt tall, 100pt wide) — both would satisfy
    // rule capture on the page, but must be suspended inside Type 3 CharProcs.
    let glyph = stream_obj(
        "",
        b"1000 0 d0\n\
          0 0 0 RG 10 w 0 500 m 1000 500 l S\n\
          0 0 0 rg 0 200 1000 10 re f",
    );
    // Page draws three Type 3 glyphs followed by one real page rule at y = 15.
    let content = b"BT /F1 100 Tf 10 30 Td (AAA) Tj ET\n\
                    0 0 0 RG 1 w 10 15 m 200 15 l S";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 340 160] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
            .to_vec(),
        b"<< /A 8 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(),
        glyph,
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut rules = Vec::new();
    let _dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_rule_sink(&mut rules)
        .interpret(&content_bytes);

    assert_eq!(
        rules.len(),
        1,
        "only the page stroke rule must be captured, not Type 3 glyph outlines: {rules:?}"
    );
    assert!(!rules[0].vertical);
    assert!((rules[0].pos - 15.0).abs() < 1e-6);
    assert!((rules[0].start - 10.0).abs() < 1e-6);
    assert!((rules[0].end - 200.0).abs() < 1e-6);
}
