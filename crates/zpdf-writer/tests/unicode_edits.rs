use std::{io::Cursor, sync::Arc};
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString};
use zpdf_document::{FieldValue, PdfDocument};
use zpdf_font::FontProvider;
use zpdf_writer::{DocumentBuilder, FormFiller, IncrementalWriter, StampItem};

fn fonts() -> Arc<FontProvider> {
    let p = Arc::new(FontProvider::memory_only());
    p.register(
        include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf").as_slice(),
        0,
    )
    .unwrap();
    p.register(
        include_bytes!("../../zpdf-font/tests/fixtures/coverage-fallback.ttf").as_slice(),
        0,
    )
    .unwrap();
    p
}
fn save(w: &IncrementalWriter) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    w.write(&mut out).unwrap();
    out.into_inner()
}
fn blank() -> Vec<u8> {
    let mut b = DocumentBuilder::new();
    b.add_page(200.0, 200.0);
    b.build().unwrap()
}
fn form(flags: i64) -> Vec<u8> {
    let mut w = IncrementalWriter::new(blank()).unwrap();
    let mut f = PdfDict::new();
    f.insert(
        PdfName::new("Subtype"),
        PdfObject::Name(PdfName::new("Widget")),
    );
    f.insert(PdfName::new("FT"), PdfObject::Name(PdfName::new("Tx")));
    f.insert(
        PdfName::new("T"),
        PdfObject::String(PdfString::new(b"name".to_vec())),
    );
    f.insert(PdfName::new("Ff"), PdfObject::Integer(flags));
    f.insert(PdfName::new("MaxLen"), PdfObject::Integer(4));
    f.insert(
        PdfName::new("DA"),
        PdfObject::String(PdfString::new(b"/Helv 12 Tf 0 g".to_vec())),
    );
    f.insert(
        PdfName::new("Rect"),
        PdfObject::Array(
            [10., 10., 190., 90.]
                .into_iter()
                .map(PdfObject::Real)
                .collect(),
        ),
    );
    let page_id = w.document().page(0).unwrap().id;
    f.insert(PdfName::new("P"), PdfObject::Ref(page_id));
    let (n, g) = w.add_object(&PdfObject::Dict(f));
    let id = ObjectId(n, g as u16);
    let mut page = w
        .resolve_current(page_id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("Annots"),
        PdfObject::Array(vec![PdfObject::Ref(id)]),
    );
    w.overwrite_object(page_id, PdfObject::Dict(page));
    let root = w.document().file().trailer.get_ref("Root").unwrap();
    let mut catalog = w.resolve_current(root).unwrap().as_dict().unwrap().clone();
    let mut af = PdfDict::new();
    af.insert(
        PdfName::new("Fields"),
        PdfObject::Array(vec![PdfObject::Ref(id)]),
    );
    af.insert(PdfName::new("NeedAppearances"), PdfObject::Bool(true));
    catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(af));
    w.overwrite_object(root, PdfObject::Dict(catalog));
    save(&w)
}
fn text(doc: &PdfDocument) -> String {
    let page = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let annotations = doc.page_annotations(&page);
    let mut spans = Vec::new();
    zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_annotations(&annotations)
        .with_text_sink(&mut spans)
        .interpret(&doc.page_content_bytes(&page).unwrap());
    zpdf_content::text::spans_to_text(spans, 2.0)
}
#[test]
fn unicode_stamp_roundtrips_with_embedded_registered_faces() {
    let mut w = IncrementalWriter::new(blank()).unwrap();
    w.set_font_provider(fonts());
    w.stamp_page(
        0,
        &[StampItem::Text {
            text: "A中伀𠀀".into(),
            x: 20.,
            y: 100.,
            font: "Helvetica".into(),
            size: 12.,
            color: (0., 0., 0.),
        }],
    )
    .unwrap();
    let doc = PdfDocument::open(save(&w)).unwrap();
    assert_eq!(text(&doc).trim(), "A中伀𠀀");
}
#[test]
fn unicode_fields_keep_values_and_appearances_including_multiline_and_comb() {
    for (flags, value) in [(0, "A中伀𠀀"), (1 << 12, "A中\n伀𠀀"), (1 << 24, "A中伀𠀀")]
    {
        let mut w = IncrementalWriter::new(form(flags)).unwrap();
        w.set_font_provider(fonts());
        let mut filler = FormFiller::new(&mut w).unwrap();
        filler.set("name", value).unwrap();
        filler.finish().unwrap();
        let doc = PdfDocument::open(save(&w)).unwrap();
        assert!(
            matches!(&doc.acro_form().unwrap().fields[0].value,Some(FieldValue::Text(s)) if s==value)
        );
        assert!(!doc.acro_form().unwrap().need_appearances);
        let actual: String = text(&doc).chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(actual, "A中伀𠀀");
        let annots = doc.page_annotations(&doc.page(0).unwrap());
        assert!(annots[0].appearance.is_some() && annots[0].generated.is_none());
    }
}
#[test]
fn uncovered_text_is_an_error_without_mutating_the_pdf() {
    let mut w = IncrementalWriter::new(form(0)).unwrap();
    w.set_font_provider(Arc::new(FontProvider::memory_only()));
    let before = save(&w);
    assert!(FormFiller::new(&mut w).unwrap().set("name", "中").is_err());
    assert_eq!(save(&w), before);
    assert!(w
        .stamp_page(
            0,
            &[StampItem::Text {
                text: "中".into(),
                x: 20.,
                y: 100.,
                font: "Helvetica".into(),
                size: 12.,
                color: (0., 0., 0.)
            }]
        )
        .is_err());
    assert_eq!(save(&w), before);
}

#[test]
fn consecutive_fill_sessions_keep_prior_unicode_appearances() {
    let mut source = IncrementalWriter::new(form(0)).unwrap();
    let first = source.document().acro_form().unwrap().fields[0].field_id;
    let mut other = source
        .resolve_current(first)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    other.insert(
        PdfName::new("T"),
        PdfObject::String(PdfString::new(b"other".to_vec())),
    );
    let (n, g) = source.add_object(&PdfObject::Dict(other));
    let second = ObjectId(n, g as u16);
    let page_id = source.document().page(0).unwrap().id;
    let mut page = source
        .resolve_current(page_id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let refs = PdfObject::Array(vec![PdfObject::Ref(first), PdfObject::Ref(second)]);
    page.insert(PdfName::new("Annots"), refs.clone());
    source.overwrite_object(page_id, PdfObject::Dict(page));
    let root = source.document().file().trailer.get_ref("Root").unwrap();
    let mut catalog = source
        .resolve_current(root)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let mut af = catalog.get_dict("AcroForm").unwrap().clone();
    af.insert(PdfName::new("Fields"), refs);
    catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(af));
    source.overwrite_object(root, PdfObject::Dict(catalog));
    let mut writer = IncrementalWriter::new(save(&source)).unwrap();
    writer.set_font_provider(fonts());
    for name in ["name", "other"] {
        let mut filler = FormFiller::new(&mut writer).unwrap();
        filler.set(name, "中伀").unwrap();
        filler.finish().unwrap();
    }
    let doc = PdfDocument::open(save(&writer)).unwrap();
    let annots = doc.page_annotations(&doc.page(0).unwrap());
    assert_eq!(annots.len(), 2);
    assert!(annots
        .iter()
        .all(|a| a.appearance.is_some() && a.generated.is_none()));
}
