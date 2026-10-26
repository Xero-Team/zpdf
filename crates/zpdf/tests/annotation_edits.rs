#![cfg(feature = "cpu-render")]
use std::{io::Cursor, sync::Arc};
use zpdf::{FontProvider, PdfDocument, Rect, RenderBackend};
use zpdf_writer::{AnnotationSpec, DocumentBuilder, IncrementalWriter};

fn blank() -> Vec<u8> {
    let mut builder = DocumentBuilder::new();
    builder.add_page(200.0, 200.0);
    builder.build().unwrap()
}
fn save(writer: &IncrementalWriter) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    out.into_inner()
}
fn render(bytes: Vec<u8>) -> impl Fn(u32, u32) -> [u8; 4] {
    let doc = PdfDocument::open(bytes).unwrap();
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let pixels = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page.fonts)
        .with_images(&page.images)
        .render_display_list(&page.display_list, 1.0)
        .unwrap();
    move |x, y| {
        let p = ((199 - y) * 200 + x) as usize * 4;
        pixels.data[p..p + 4].try_into().unwrap()
    }
}

#[test]
fn saved_appearance_reflects_successive_colour_and_border_edits() {
    let mut writer = IncrementalWriter::new(blank()).unwrap();
    let id = writer
        .add_annotation(
            0,
            &AnnotationSpec::Square {
                rect: Rect::new(20.0, 20.0, 100.0, 100.0),
                color: (0.0, 0.0, 0.0),
                interior: None,
                width: 1.0,
            },
        )
        .unwrap();
    writer.update_annotation_color(id, (1.0, 0.0, 0.0)).unwrap();
    writer.update_annotation_border_width(id, 8.0).unwrap();
    let px = render(save(&writer));
    assert_eq!(px(23, 50), [255, 0, 0, 255]);
    assert_eq!(px(50, 50), [255, 255, 255, 255]);
    writer.update_annotation_border_width(id, 0.0).unwrap();
    let px = render(save(&writer));
    assert_eq!(px(23, 50), [255, 255, 255, 255]);
}

#[test]
fn moved_line_stays_moved_when_its_appearance_is_regenerated() {
    let mut writer = IncrementalWriter::new(blank()).unwrap();
    let id = writer
        .add_annotation(
            0,
            &AnnotationSpec::Line {
                x1: 10.0,
                y1: 10.0,
                x2: 50.0,
                y2: 50.0,
                color: (0.0, 0.0, 0.0),
                width: 4.0,
            },
        )
        .unwrap();
    let bytes = save(&writer);
    let doc = PdfDocument::open(bytes.clone()).unwrap();
    let r = doc.page_annotations(&doc.page(0).unwrap())[0].rect;
    let mut writer = IncrementalWriter::new(bytes).unwrap();
    writer
        .update_annotation_rect(id, Rect::new(r.x0 + 100.0, r.y0, r.x1 + 100.0, r.y1))
        .unwrap();
    writer.update_annotation_color(id, (1.0, 0.0, 0.0)).unwrap();
    let px = render(save(&writer));
    assert_eq!(px(130, 30), [255, 0, 0, 255]);
    assert_eq!(px(30, 30), [255, 255, 255, 255]);
}

#[test]
fn malformed_geometry_does_not_erase_a_stored_appearance() {
    let mut writer = IncrementalWriter::new(blank()).unwrap();
    let id = writer
        .add_annotation(
            0,
            &AnnotationSpec::Line {
                x1: 10.0,
                y1: 10.0,
                x2: 50.0,
                y2: 50.0,
                color: (1.0, 0.0, 0.0),
                width: 4.0,
            },
        )
        .unwrap();
    let mut dict = writer
        .resolve_current(id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    dict.0.remove("L");
    writer.overwrite_object(id, zpdf::PdfObject::Dict(dict));
    let before = save(&writer);
    assert!(writer.update_annotation_color(id, (0.0, 1.0, 0.0)).is_err());
    assert_eq!(save(&writer), before);
    assert_eq!(render(save(&writer))(30, 30), [255, 0, 0, 255]);
}

#[test]
fn freetext_replacement_embeds_unicode_and_rejects_missing_coverage_without_mutation() {
    let mut writer = IncrementalWriter::new(blank()).unwrap();
    let id = writer
        .add_annotation(
            0,
            &AnnotationSpec::FreeText {
                rect: Rect::new(20.0, 20.0, 180.0, 180.0),
                contents: "OLD".into(),
                size: Some(16.0),
                color: None,
            },
        )
        .unwrap();
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register(
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf").as_slice(),
            0,
        )
        .unwrap();
    writer.set_font_provider(provider);
    writer.update_annotation_contents(id, "中").unwrap();
    let saved = save(&writer);
    let doc = PdfDocument::open(saved.clone()).unwrap();
    let page = doc.page(0).unwrap();
    let annotations = doc.page_annotations(&page);
    let mut fonts = doc.load_page_fonts(&page);
    let mut spans = Vec::new();
    zpdf::ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_annotations(&annotations)
        .with_text_sink(&mut spans)
        .interpret(b"");
    assert_eq!(
        spans.iter().map(|s| s.text.as_str()).collect::<String>(),
        "中"
    );
    assert!(writer.update_annotation_contents(id, "文").is_err());
    assert_eq!(save(&writer), saved);
    assert!(writer
        .update_annotation_color(id, (f64::NAN, 0.0, 0.0))
        .is_err());
    assert!(writer
        .update_annotation_border_width(id, f64::INFINITY)
        .is_err());
    assert_eq!(save(&writer), saved);
}
