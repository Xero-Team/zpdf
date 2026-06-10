//! A form XObject with unbalanced `Q` operators must not corrupt the
//! page-level graphics state (color, CTM, clip pairing) after the form.

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::{Paint, RenderCommand};
use zpdf_document::PdfDocument;

/// Hand-roll a minimal PDF: page draws red, invokes a form whose body sets
/// green and pops the stack three times too many, then fills a rect.
fn build_pdf() -> Vec<u8> {
    let form_body: &[u8] = b"q 0 1 0 rg Q Q Q Q 0 0 1 rg";
    let content: &[u8] = b"1 0 0 rg /Fm1 Do 10 10 50 50 re f";

    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /XObject << /Fm1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, {
            let mut v = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            v.extend_from_slice(content);
            v.extend_from_slice(b"\nendstream");
            v
        }),
        (5, {
            let mut v = format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 200 200] /Length {} >>\nstream\n",
                form_body.len()
            )
            .into_bytes();
            v.extend_from_slice(form_body);
            v.extend_from_slice(b"\nendstream");
            v
        }),
    ];

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in &objs {
        offsets.push((*num, out.len()));
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_off = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for (_, off) in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objs.len() + 1,
            xref_off
        )
        .as_bytes(),
    );
    out
}

#[test]
fn unbalanced_q_in_form_does_not_leak_state() {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");

    let list = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_document(doc.file(), &page.resources)
        .interpret(&content);

    // The rect fill after the form must still paint page-level red, not the
    // form's green/blue, and clip pushes/pops must balance.
    let mut fills = Vec::new();
    let mut clip_depth = 0i32;
    let mut min_clip_depth = 0i32;
    for cmd in &list.commands {
        match cmd {
            RenderCommand::FillPath { paint, .. } => fills.push(paint.clone()),
            RenderCommand::PushClip { .. } => clip_depth += 1,
            RenderCommand::PopClip => {
                clip_depth -= 1;
                min_clip_depth = min_clip_depth.min(clip_depth);
            }
            _ => {}
        }
    }
    let Some(Paint::Solid(c)) = fills.last() else {
        panic!("expected a solid fill, got {:?}", fills.last());
    };
    assert!(
        c.r > 0.9 && c.g < 0.1 && c.b < 0.1,
        "page fill color leaked from form: {c:?}"
    );
    assert_eq!(clip_depth, 0, "unbalanced clip commands");
    assert!(min_clip_depth >= 0, "PopClip underflow");
}
