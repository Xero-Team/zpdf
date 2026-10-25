//! Text clipping (render modes 4–7) acceptance on the CPU backend: what an
//! `ET` installs when the clipped text produced no outline. Blank glyphs with
//! known outlines clip out everything, like an empty `W n` path; glyphs with no
//! outline source (Type3 CharProcs, no selected font) leave the clip unchanged
//! instead of hiding all later painting. Pattern-filled text clips the pattern
//! to the same outlines, so outline-less runs must still be painted.
#![cfg(feature = "cpu-render")]

use std::sync::Arc;

use zpdf::{ContentInterpreter, FontProvider, ImageCache, PdfDocument, RenderBackend};

/// Real embedded TrueType font: its cmap maps 'A' (0x41) to a filled
/// rectangle, while the glyph shown for a space has no contours.
const TEST_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");

/// Paint after the text object: red over the whole page inside the same q…Q,
/// then a small green square after Q to prove the clip is popped.
const PAINT_AFTER: &[u8] = b" 1 0 0 rg 0 0 200 200 re f Q 0 1 0 rg 190 0 10 10 re f";

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

fn page_with_font(content: &[u8], font: Vec<Vec<u8>>, after: &[u8]) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> /ExtGState << /GS << /ca 0.25 /CA 0.5 >> >> >> >>"
            .to_vec(),
        stream_obj("", &[content, after].concat()),
    ];
    objs.extend(font);
    assemble(&objs)
}

/// Type3 glyph 'sq' (code 65): a filled square drawn by its CharProc.
fn type3_pdf(content: &[u8]) -> Vec<u8> {
    page_with_font(
        content,
        vec![
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
              /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
                .to_vec(),
            b"<< /sq 8 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [65 /sq] >>".to_vec(),
            stream_obj("", b"1000 0 d0\n150 150 700 700 re\nf"),
        ],
        PAINT_AFTER,
    )
}

fn truetype_pdf(content: &[u8]) -> Vec<u8> {
    truetype_pdf_with_tail(content, PAINT_AFTER)
}

fn truetype_pdf_with_tail(content: &[u8], after: &[u8]) -> Vec<u8> {
    page_with_font(
        content,
        vec![
            b"<< /Type /Font /Subtype /TrueType /BaseFont /ZpdfSans /FirstChar 32 \
              /LastChar 32 /Widths [500] /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ZpdfSans /Flags 32 \
              /FontBBox [0 -200 700 800] /FontFile2 7 0 R >>"
                .to_vec(),
            stream_obj(&format!("/Length1 {}", TEST_TTF.len()), TEST_TTF),
        ],
        after,
    )
}

/// Render page 0 at 1 px/pt; returns RGB at the given top-left pixel coords.
fn render(pdf: Vec<u8>) -> impl Fn(u32, u32) -> [u8; 3] {
    render_document(PdfDocument::open(pdf).expect("open pdf"))
}

fn render_document(doc: PdfDocument) -> impl Fn(u32, u32) -> [u8; 3] {
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    let image = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    move |x, y| {
        let i = ((y * image.width + x) * 4) as usize;
        [image.data[i], image.data[i + 1], image.data[i + 2]]
    }
}

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const WHITE: [u8; 3] = [255, 255, 255];

#[test]
fn text_stroke_modes_paint_edges_separately_from_interior() {
    for mode in [1, 2, 5, 6] {
        let content = format!("q 0 1 0 rg 1 0 0 RG 4 w BT /F1 60 Tf 15 70 Td {mode} Tr (A) Tj ET");
        let px = render(truetype_pdf_with_tail(content.as_bytes(), b" Q"));
        assert_eq!(px(20, 110), RED, "mode {mode}: outside half of stroke");
        assert_eq!(
            px(30, 110),
            if mode == 2 || mode == 6 { GREEN } else { WHITE },
            "mode {mode}: interior"
        );
        assert_eq!(px(18, 110), WHITE, "mode {mode}: outside stroke");
    }
}

#[test]
fn text_strokes_use_stroke_alpha_and_user_space_width() {
    let px = render(truetype_pdf_with_tail(
        b"q /GS gs 0 1 0 rg 1 0 0 RG 4 w BT /F1 60 Tf 2 0 0 2 15 70 Tm 2 Tr (A) Tj ET",
        b" Q",
    ));
    assert_eq!(
        px(23, 100),
        WHITE,
        "text matrix must not double the line width"
    );
    for (actual, expected) in [
        (px(25, 100), [255, 127, 127]),
        (px(45, 100), [191, 255, 191]),
    ] {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(a, b)| (*a as i32 - b).abs() <= 1),
            "{actual:?} versus {expected:?}"
        );
    }
}

#[test]
fn stroke_and_clip_modes_clip_to_glyph_interior_at_et() {
    for mode in [5, 6] {
        let content = format!("q 1 0 0 RG 4 w BT /F1 60 Tf 15 70 Td {mode} Tr (A) Tj ET");
        let px = render(truetype_pdf_with_tail(
            content.as_bytes(),
            b" 0 0 1 rg 0 0 200 200 re f Q",
        ));
        assert_eq!(px(30, 110), [0, 0, 255]);
        assert_eq!(
            px(20, 110),
            RED,
            "stroke outside the outline survives later clipped fill"
        );
        assert_eq!(px(100, 100), WHITE);
    }
}

#[test]
fn blank_glyph_text_clip_hides_later_paint_until_q() {
    let px = render(truetype_pdf(b"q BT /F1 60 Tf 20 70 Td 7 Tr ( ) Tj ET"));
    assert_eq!(px(100, 100), WHITE, "an empty text clip exposes nothing");
    assert_eq!(px(195, 195), GREEN, "Q pops the empty text clip");
}

#[test]
fn outlined_glyph_text_clip_confines_later_paint() {
    // 'A' is a rectangle at x 21..39, y 88..130 in device pixels at 60 pt.
    let px = render(truetype_pdf(b"q BT /F1 60 Tf 15 70 Td 7 Tr (A) Tj ET"));
    assert_eq!(px(30, 110), RED, "inside the glyph");
    assert_eq!(px(100, 100), WHITE, "outside the glyph");
    assert_eq!(px(195, 195), GREEN);
}

#[test]
fn type3_text_clip_without_outlines_leaves_clip_unchanged() {
    // Type3 CharProcs are content streams, not outlines; an empty clip here
    // would erase everything painted up to the enclosing Q.
    let px = render(type3_pdf(b"q BT /F1 60 Tf 15 70 Td 7 Tr (A) Tj ET"));
    assert_eq!(px(100, 100), RED);
    assert_eq!(px(195, 195), GREEN);
}

#[test]
fn clip_mode_text_without_a_font_leaves_clip_unchanged() {
    let px = render(truetype_pdf(b"q BT 15 70 Td 7 Tr (A) Tj ET"));
    assert_eq!(px(100, 100), RED);
    assert_eq!(px(195, 195), GREEN);
}

#[test]
fn empty_string_in_clip_mode_installs_no_clip() {
    let px = render(truetype_pdf(b"q BT /F1 60 Tf 7 Tr () Tj ET"));
    assert_eq!(px(100, 100), RED);
}

#[test]
fn empty_path_clip_hides_later_paint_until_q() {
    let px = render(truetype_pdf(b"q W n"));
    assert_eq!(px(100, 100), WHITE);
    assert_eq!(px(195, 195), GREEN);
}

#[test]
fn pattern_text_keeps_type3_runs_beside_fallback_glyphs() {
    // Code 65 draws the Type3 square; code 66 names a glyph without a
    // CharProc, so a registered TrueType face supplies it. The pattern is
    // clipped to that face's outline, and the Type3 run, which has no outline
    // to clip with, must still be painted instead of being dropped.
    let pattern = b"1 0 0 rg 0 0 10 10 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> /Pattern << /P 9 0 R >> >> >>"
            .to_vec(),
        stream_obj(
            "",
            b"0 0 1 rg /Pattern cs /P scn BT /F1 60 Tf 15 70 Td (AB) Tj ET",
        ),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 66 /Widths [1000 1000] /Resources << >> >>"
            .to_vec(),
        b"<< /sq 8 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [65 /sq /B] >>".to_vec(),
        stream_obj("", b"1000 0 d0\n150 150 700 700 re\nf"),
        stream_obj(
            "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] \
             /XStep 10 /YStep 10 /Resources << >>",
            pattern,
        ),
    ]);
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register(
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf").as_slice(),
            0,
        )
        .unwrap();
    let doc = PdfDocument::open(pdf).unwrap().with_font_provider(provider);
    let px = render_document(doc);
    // The Type3 square spans x 24..57, y 79..112 in PDF space.
    assert_ne!(px(40, 100), WHITE, "the Type3 run is painted");
    let pattern_ink = (60..120)
        .flat_map(|x| (60..140).map(move |y| (x, y)))
        .any(|(x, y)| px(x, y) == RED);
    assert!(pattern_ink, "the fallback glyph shows the pattern");
}
