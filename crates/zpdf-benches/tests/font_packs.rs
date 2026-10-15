use std::sync::Arc;

use zpdf::{FontProvider, PdfDocument};
use zpdf_benches::{font_pack_workloads as fixtures, font_workloads};

#[test]
fn lazy_real_cjk_font_renders_headlessly_with_original_pdf_metrics() {
    let encoded = fixtures::compress(fixtures::CJK);
    let pack = fixtures::pack(encoded, fixtures::CJK.len() as u64);
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_pack(Arc::clone(&pack), 0, &[fixtures::ALIAS])
        .unwrap();
    let doc = PdfDocument::open(fixtures::pdf())
        .unwrap()
        .with_font_provider(provider);
    assert!(!pack.is_decoded());
    let setup = fixtures::assert_cjk(&doc);
    assert!(pack.is_decoded());
    let svg = zpdf_svg_export::display_list_to_svg(
        &setup.dl,
        &setup.font_cache,
        &setup.image_cache,
        &zpdf_svg_export::SvgOptions::default(),
    );
    // SVG batches the run into one path. The independently inspected fixture
    // has 3, 4, 6 and 7 contours respectively; all twenty must survive export.
    let path = svg
        .split_once("<path d=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    assert_eq!(path.matches('M').count(), 20);
    assert_eq!(path.matches('Z').count(), 20);
}

#[test]
fn late_font_pack_registration_refreshes_a_missing_face() {
    let provider = Arc::new(FontProvider::memory_only());
    let doc = PdfDocument::open(fixtures::pdf())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let before = font_workloads::setup(&doc, 144.0);
    assert!(!before
        .font_cache
        .get_by_name("F1")
        .unwrap()
        .1
        .has_font_data());
    provider
        .register_pack(
            fixtures::pack(
                fixtures::compress(fixtures::CJK),
                fixtures::CJK.len() as u64,
            ),
            0,
            &[fixtures::ALIAS],
        )
        .unwrap();
    fixtures::assert_cjk(&doc);
    assert!(!before
        .font_cache
        .get_by_name("F1")
        .unwrap()
        .1
        .has_font_data());
}
