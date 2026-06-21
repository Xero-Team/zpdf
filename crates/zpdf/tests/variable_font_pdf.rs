//! End-to-end: a PDF that embeds a variable TrueType font and sets `/FontWeight`
//! in its FontDescriptor must render the heavier instance — the loader maps
//! `/FontWeight` → the `wght` variation axis. Reuses the `fvar`/`gvar` fixture
//! from zpdf-font's unit test (glyph `A`'s right edge moves 400→700 as weight
//! goes 400→900).

use zpdf::PdfDocument;
use zpdf_font::OutlineCommand;

const VAR_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");

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

fn pdf_with_weight(weight: Option<i64>) -> Vec<u8> {
    let fw = weight
        .map(|w| format!(" /FontWeight {w}"))
        .unwrap_or_default();
    let descriptor = format!(
        "<< /Type /FontDescriptor /FontName /ZpdfVar /Flags 32{fw} \
         /FontBBox [0 -200 700 800] /FontFile2 6 0 R >>"
    );
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
          /Resources << /Font << /F1 4 0 R >> >> >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ZpdfVar /FirstChar 65 \
          /LastChar 65 /Widths [500] /FontDescriptor 5 0 R >>"
            .to_vec(),
        descriptor.into_bytes(),
        stream_obj(&format!("/Length1 {}", VAR_TTF.len()), VAR_TTF),
    ])
}

/// Maximum x of glyph `A`'s (GID 1) outline, as loaded from the PDF.
fn right_edge(weight: Option<i64>) -> f64 {
    let doc = PdfDocument::open(pdf_with_weight(weight)).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let fonts = doc.load_page_fonts(&page);
    let (_, font) = fonts.get_by_name("F1").expect("font F1");
    let outline = font.glyph_outline(1).expect("glyph outline A");
    outline.commands.iter().fold(f64::MIN, |m, c| match *c {
        OutlineCommand::MoveTo(x, _) | OutlineCommand::LineTo(x, _) => m.max(x),
        OutlineCommand::QuadTo(x1, _, x2, _) => m.max(x1).max(x2),
        OutlineCommand::CurveTo(x1, _, x2, _, x3, _) => m.max(x1).max(x2).max(x3),
        OutlineCommand::Close => m,
    })
}

#[test]
fn font_weight_descriptor_drives_wght_axis() {
    let regular = right_edge(None); // no /FontWeight → default master
    let bold = right_edge(Some(900)); // /FontWeight 900 → heavy instance

    assert!(
        (regular - 400.0).abs() < 1.0,
        "no /FontWeight → default master right edge ~400, got {regular}"
    );
    assert!(
        bold > 650.0,
        "/FontWeight 900 → wght axis widens 'A' toward 700, got {bold}"
    );
}
