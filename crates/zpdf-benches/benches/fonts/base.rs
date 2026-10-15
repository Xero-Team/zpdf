//! Font loading, mapping and outline microbenchmarks plus end-to-end controls.
//! Every case validates its glyphs first; no timing depends on installed fonts
//! or network I/O.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion};
use zpdf::{PdfDocument, RenderBackend};
use zpdf_benches::font_workloads as fixtures;
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

/// The font draws `gid` with a real outline. A failed parse still constructs a
/// widths-only font, so construction alone proves nothing.
fn assert_draws(font: &LoadedFont, gid: u16) {
    assert!(
        font.glyph_outline(gid)
            .is_some_and(|outline| !outline.commands.is_empty()),
        "{} cannot draw glyph {gid}",
        font.base_font
    );
}

pub(crate) fn loading(c: &mut Criterion) {
    let mut group = c.benchmark_group("fonts/load");
    let primary = fixtures::primary_font();
    assert_draws(&primary, primary.unicode_glyph('A' as u32).unwrap().0);
    group.bench_function("truetype", |b| {
        b.iter(|| black_box(fixtures::primary_font()))
    });
    let variable = || {
        LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "ZpdfVar".into(),
            fixtures::VARIABLE.to_vec(),
            CidWidths::new(1000.0),
        )
    };
    let font = variable();
    assert_draws(&font, font.unicode_glyph('A' as u32).unwrap().0);
    group.bench_function("variable", |b| b.iter(|| black_box(variable())));
    let cff = fixtures::quartz_font();
    assert_draws(&cff, cff.code_to_gid(b'U' as u16).unwrap());
    group.bench_function("cff_pdf", |b| b.iter(|| black_box(fixtures::quartz_font())));
    // Page loads substitute placeholders for fonts that fail to load, so check
    // the drawn text and every page's fonts before timing either document case.
    let text = "A".repeat(256);
    let pdf = fixtures::make_pdf(&text, true, 12);
    let doc = PdfDocument::open(Arc::clone(&pdf)).unwrap();
    fixtures::assert_text_case(&doc, &text, 150.0);
    for index in 0..12 {
        let page = doc.page(index).unwrap();
        let fonts = doc.load_page_fonts(&page);
        for name in page.resources.fonts.keys() {
            let (_, font) = fonts.get_by_name(name).expect("page font");
            assert!(
                font.has_font_data(),
                "page {index} font {name} is a placeholder"
            );
        }
    }
    group.bench_function("document_cold_12_pages", |b| {
        b.iter_batched(
            || PdfDocument::open(Arc::clone(&pdf)).unwrap(),
            |doc| {
                for index in 0..12 {
                    black_box(doc.load_page_fonts(&doc.page(index).unwrap()));
                }
            },
            BatchSize::SmallInput,
        )
    });
    // The validation above already loaded every page of `doc`, so it is warm.
    group.bench_function("document_warm_12_pages", |b| {
        b.iter(|| {
            for index in 0..12 {
                black_box(doc.load_page_fonts(&doc.page(index).unwrap()));
            }
        })
    });
    group.finish();
}

pub(crate) fn mapping(c: &mut Criterion) {
    let font = fixtures::primary_font();
    let mut group = c.benchmark_group("fonts/map");
    for (name, codes) in [
        ("latin_256", vec![65; 256]),
        ("cjk_256", (0x4E00..0x4F00).collect()),
        (
            "supplementary_256",
            (0..256).map(|i| 0x20000 + i % 16).collect(),
        ),
        ("missing_256", vec![0x10FFFD; 256]),
    ] {
        for &code in &codes {
            assert_eq!(font.unicode_glyph(code).is_some(), name != "missing_256");
        }
        group.bench_function(name, |b| {
            b.iter(|| {
                for &code in &codes {
                    black_box(font.unicode_glyph(black_box(code)));
                }
            })
        });
    }
    for (name, code, expected) in [
        ("GBpc-EUC-H", 0xCFC2, 0x4E0B),
        ("GBK-EUC-H", 0xD2BB, 0x4E00),
        ("B5pc-H", 0xA440, 0x4E00),
        ("90ms-RKSJ-H", 0x889F, 0x4E9C),
        ("EUC-H", 0xB0A1, 0x4E9C),
        ("KSC-EUC-H", 0xB0A1, 0xAC00),
    ] {
        let map = zpdf_font::cmap::CidCMap::predefined(name).unwrap();
        assert_eq!(map.decode_to_unicode(code, 2), Some(expected));
        group.bench_function(name, |b| {
            b.iter(|| {
                for _ in 0..256 {
                    black_box(map.decode_to_unicode(black_box(code), 2));
                }
            })
        });
    }
    group.finish();
}

pub(crate) fn outlines(c: &mut Criterion) {
    let font = fixtures::primary_font();
    let mut group = c.benchmark_group("fonts/outline");
    for (name, codes) in [
        ("repeated_256", vec![65; 256]),
        ("unique_256", (0x4E00..0x4F00).collect()),
    ] {
        let gids: Vec<_> = codes
            .iter()
            .map(|&cp| font.unicode_glyph(cp).unwrap().0)
            .collect();
        assert!(gids.iter().all(|&gid| font
            .glyph_outline(gid)
            .is_some_and(|outline| !outline.commands.is_empty())));
        group.bench_function(name, |b| {
            b.iter(|| {
                for &gid in &gids {
                    black_box(font.glyph_outline(black_box(gid)));
                }
            })
        });
    }
    let cff = fixtures::quartz_font();
    let gid = cff.code_to_gid(b'U' as u16).unwrap();
    assert_eq!(gid, 26);
    assert!(cff.glyph_outline(gid).is_some());
    group.bench_function("cff_256", |b| {
        b.iter(|| {
            for _ in 0..256 {
                black_box(cff.glyph_outline(black_box(gid)));
            }
        })
    });
    let mut variable = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "ZpdfVar".into(),
        fixtures::VARIABLE.to_vec(),
        CidWidths::new(1000.0),
    );
    let gid = variable.unicode_glyph('A' as u32).unwrap().0;
    let default = variable.glyph_outline(gid).unwrap();
    variable.set_variations(Some(900.0), None, None, false);
    // The timed outline must exist and come from the configured instance, so a
    // broken variation path cannot read as a speedup.
    let configured = variable.glyph_outline(gid).unwrap();
    assert!(!configured.commands.is_empty());
    assert_ne!(
        format!("{:?}", configured.commands),
        format!("{:?}", default.commands)
    );
    group.bench_function("variable_256", |b| {
        b.iter(|| {
            for _ in 0..256 {
                black_box(variable.glyph_outline(black_box(gid)));
            }
        })
    });
    group.finish();
}

pub(crate) fn pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("fonts/pipeline");
    for (name, text, composite) in fixtures::text_cases() {
        let pdf = fixtures::make_pdf(&text, composite, 1);
        let doc = PdfDocument::open(Arc::clone(&pdf)).unwrap();
        group.bench_function(BenchmarkId::new("extract", name), |b| {
            b.iter(|| black_box(fixtures::extract(&doc)))
        });
        group.bench_function(BenchmarkId::new("writer", name), |b| {
            b.iter(|| black_box(fixtures::make_pdf(&text, composite, 1)))
        });
        for dpi in [96.0, 150.0, 300.0] {
            fixtures::assert_text_case(&doc, &text, dpi);
            let setup = fixtures::setup(&doc, dpi);
            group.bench_function(BenchmarkId::new("render", format!("{name}@{dpi}")), |b| {
                b.iter_batched(
                    || zpdf::cpu::CpuRenderer::new().with_fonts(&setup.font_cache),
                    |mut renderer| {
                        black_box(
                            renderer
                                .render_display_list(&setup.dl, setup.scale)
                                .unwrap(),
                        )
                    },
                    BatchSize::SmallInput,
                )
            });
            group.bench_function(BenchmarkId::new("total", format!("{name}@{dpi}")), |b| {
                b.iter(|| {
                    let doc = PdfDocument::open(Arc::clone(&pdf)).unwrap();
                    let setup = fixtures::setup(&doc, dpi);
                    let mut renderer = zpdf::cpu::CpuRenderer::new().with_fonts(&setup.font_cache);
                    black_box(
                        renderer
                            .render_display_list(&setup.dl, setup.scale)
                            .unwrap(),
                    )
                })
            });
        }
    }
    for dpi in [96.0, 150.0, 300.0] {
        let setup = fixtures::cff_setup(dpi);
        group.bench_function(BenchmarkId::new("render_cff", dpi), |b| {
            b.iter_batched(
                || zpdf::cpu::CpuRenderer::new().with_fonts(&setup.font_cache),
                |mut renderer| {
                    black_box(
                        renderer
                            .render_display_list(&setup.dl, setup.scale)
                            .unwrap(),
                    )
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Type1 program construction and variable-font Unicode lookup, the two
/// constructor paths the TrueType `load` cases do not cover.
pub(crate) fn type1_variable(c: &mut Criterion) {
    let mut group = c.benchmark_group("fonts/type1_variable");
    let type1 = include_bytes!("../../../zpdf-font/tests/fixtures/coverage-type1.pfa");
    let load_type1 = || {
        LoadedFont::new_with_data(
            PdfFontType::Type1,
            "ZpdfCoverageType1".into(),
            type1.to_vec(),
            CidWidths::new(1000.0),
        )
    };
    let font = load_type1();
    assert!(font.type1.is_some());
    let gid = font.code_to_gid(b'A' as u16).unwrap();
    assert_eq!(gid, 1);
    assert_eq!(font.glyph_outline(gid).unwrap().advance_width, 1000.0);
    group.bench_function("type1_construct", |b| b.iter(|| black_box(load_type1())));
    for configured in [false, true] {
        let mut font = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "VariableLookup".into(),
            fixtures::VARIABLE.to_vec(),
            CidWidths::new(1000.0),
        );
        let mut face = ttf_parser::Face::parse(fixtures::VARIABLE, 0).unwrap();
        if configured {
            font.set_variations(Some(900.0), None, None, false);
            face.set_variation(ttf_parser::Tag::from_bytes(b"wght"), 900.0);
        }
        let gid = face.glyph_index('A').unwrap();
        let expected = (
            gid.0,
            face.glyph_hor_advance(gid).unwrap() as f64 * 1000.0 / face.units_per_em() as f64,
        );
        assert_eq!(font.unicode_glyph('A' as u32), Some(expected));
        let name = if configured {
            "unicode_variable/configured_256"
        } else {
            "unicode_variable/default_256"
        };
        group.bench_function(name, |b| {
            b.iter(|| {
                for _ in 0..256 {
                    black_box(font.unicode_glyph(black_box('A' as u32)));
                }
            })
        });
    }
    group.finish();
}
