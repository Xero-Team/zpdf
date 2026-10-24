//! An ExtGState `/Font [font-ref size]` entry selects the font for following
//! text exactly as `Tf` would.

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::RenderCommand;
use zpdf_document::PdfDocument;
use zpdf_font::FontCache;

fn build_pdf() -> Vec<u8> {
    let content: &[u8] = b"BT /GS0 gs 10 10 Td (Hi) Tj ET";
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /ExtGState << /GS0 << /Font [5 0 R 24] >> >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, stream),
        (
            5,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in &objs {
        offsets.push(out.len());
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[test]
fn ext_gstate_font_selects_the_font_and_size() {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");
    let mut cache = FontCache::new();
    let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_fonts(&mut cache)
        .with_document(doc.file(), &page.resources)
        .interpret(&content);
    let run = dl
        .commands
        .iter()
        .find_map(|c| match c {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .expect("the text is drawn in the ExtGState's font");
    assert_eq!(run.font_size, 24.0);
    assert_eq!(run.glyphs.len(), 2);
}
