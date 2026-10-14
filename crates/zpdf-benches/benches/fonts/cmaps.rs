//! Authoritative CMap lookup, collection substitution and end-to-end controls.

use criterion::Criterion;
use std::{hint::black_box, sync::Arc};
use zpdf::{FontProvider, PdfDocument, RenderBackend};
use zpdf_benches::{font_cmap_workloads as fixtures, font_workloads};
use zpdf_font::cmap::CidCMap;
use zpdf_writer::{DocumentBuilder, PredefinedOrdering};

pub(crate) fn cmaps(c: &mut Criterion) {
    for &case in fixtures::CASES {
        let mut cmap = CidCMap::predefined(case.encoding).unwrap();
        cmap.set_collection(case.ordering);
        let (code, length) = cmap.next_code(case.code);
        assert_eq!(cmap.code_to_cid(code, length as u8), case.cid as u32);
        assert_eq!(
            cmap.decode_to_unicode(code, length as u8),
            Some(case.character as u32)
        );
        c.bench_function(&format!("font_cmaps/lookup/{}", case.name), |b| {
            b.iter(|| {
                for _ in 0..256 {
                    black_box(cmap.code_to_cid(black_box(code), length as u8));
                }
            })
        });
        c.bench_function(&format!("font_cmaps/decode/{}", case.name), |b| {
            b.iter(|| {
                for _ in 0..256 {
                    let (code, length) = cmap.next_code(black_box(case.code));
                    black_box(cmap.decode_text(code, length as u8));
                }
            })
        });
        for embedded in [false, true] {
            let provider = Arc::new(FontProvider::memory_only());
            if !embedded {
                provider
                    .register_with_aliases(fixtures::FONT, 0, &["CMapFixture"])
                    .unwrap();
            }
            let bytes = fixtures::pdf(case, embedded, 8);
            let document = PdfDocument::open(Arc::clone(&bytes))
                .unwrap()
                .with_font_provider(Arc::clone(&provider));
            fixtures::assert_case(&document, case, embedded, 8);
            let mode = if embedded { "embedded" } else { "substitute" };
            c.bench_function(&format!("font_cmaps/pipeline/{}/{mode}", case.name), |b| {
                b.iter(|| {
                    let doc = PdfDocument::open(Arc::clone(&bytes))
                        .unwrap()
                        .with_font_provider(Arc::clone(&provider));
                    let setup = font_workloads::setup(&doc, 144.0);
                    black_box(
                        zpdf::cpu::CpuRenderer::new()
                            .with_fonts(&setup.font_cache)
                            .render_display_list(&setup.dl, setup.scale)
                            .unwrap(),
                    );
                })
            });
        }
    }
    for (name, cff, vertical) in [
        ("predefined_h", false, false),
        ("predefined_v", false, true),
        ("cff_nonidentity", true, false),
    ] {
        c.bench_function(&format!("font_cmaps/writer/{name}"), |b| {
            b.iter(|| {
                let mut builder = DocumentBuilder::new();
                let font = if cff {
                    builder.embed_composite_font(fixtures::CID_CFF.to_vec())
                } else {
                    builder.embed_composite_font_predefined(
                        fixtures::FONT.to_vec(),
                        PredefinedOrdering::Gb,
                        vertical,
                    )
                }
                .unwrap();
                let page = builder.add_page(240.0, 280.0);
                builder
                    .add_text_embedded(page, "A中", 20.0, 50.0, font, 10.0, (0.0, 0.0, 0.0))
                    .unwrap();
                black_box(builder.build().unwrap());
            })
        });
    }
}
