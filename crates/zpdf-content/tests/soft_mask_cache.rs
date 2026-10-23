//! Content that repeats the same soft-mask ExtGState per object (common in
//! Illustrator/PowerPoint exports) builds the mask group once.

use std::sync::Arc;

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::{RenderCommand, SoftMask};
use zpdf_document::PdfDocument;

fn build_pdf(content: &[u8]) -> Vec<u8> {
    let stream = |dict: &str, data: &[u8]| {
        let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    };
    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /ExtGState << /GS1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, stream("", content)),
        (
            5,
            b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 6 0 R >> >>".to_vec(),
        ),
        (
            6,
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
                 /Group << /S /Transparency /CS /DeviceGray >>",
                b"1 g 0 0 100 100 re f",
            ),
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

/// The soft masks attached to the page's blend groups, in order.
fn masks(content: &[u8]) -> Vec<SoftMask> {
    let doc = PdfDocument::open(build_pdf(content)).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let bytes = doc.page_content_bytes(&page).expect("content");
    ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_document(doc.file(), &page.resources)
        .interpret(&bytes)
        .commands
        .into_iter()
        .filter_map(|c| match c {
            RenderCommand::PushBlendGroup { mask: Some(m), .. } => Some(m),
            _ => None,
        })
        .collect()
}

#[test]
fn a_repeated_soft_mask_gs_builds_the_mask_once() {
    let masks = masks(b"/GS1 gs 0 0 10 10 re f /GS1 gs 20 0 10 10 re f");
    assert_eq!(masks.len(), 2);
    assert!(Arc::ptr_eq(&masks[0].commands, &masks[1].commands));
}

#[test]
fn a_soft_mask_under_another_ctm_is_built_again() {
    let masks = masks(b"/GS1 gs 0 0 10 10 re f q 2 0 0 2 0 0 cm /GS1 gs 0 0 10 10 re f Q");
    assert_eq!(masks.len(), 2);
    assert!(!Arc::ptr_eq(&masks[0].commands, &masks[1].commands));
}
