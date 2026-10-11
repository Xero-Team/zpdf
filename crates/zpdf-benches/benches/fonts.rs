//! Font microbenchmarks plus portable end-to-end controls.
//!
//! The runner in tools/font_baseline.py checks exact case coverage and records
//! host/fixture identity. No timing depends on installed fonts or network I/O.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use zpdf::{PdfDocument, RenderBackend};
use zpdf_benches::font_workloads as fixtures;
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

fn loading(c: &mut Criterion) {
    let mut group = c.benchmark_group("fonts/load");
    group.bench_function("truetype", |b| {
        b.iter(|| black_box(fixtures::primary_font()))
    });
    group.bench_function("variable", |b| {
        b.iter(|| {
            black_box(LoadedFont::new_with_data(
                PdfFontType::TrueType,
                "ZpdfVar".into(),
                fixtures::VARIABLE.to_vec(),
                CidWidths::new(1000.0),
            ))
        })
    });
    group.bench_function("cff_pdf", |b| b.iter(|| black_box(fixtures::quartz_font())));
    let pdf = fixtures::make_pdf(&"A".repeat(256), true, 12);
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
    let doc = PdfDocument::open(pdf).unwrap();
    for index in 0..12 {
        black_box(doc.load_page_fonts(&doc.page(index).unwrap()));
    }
    group.bench_function("document_warm_12_pages", |b| {
        b.iter(|| {
            for index in 0..12 {
                black_box(doc.load_page_fonts(&doc.page(index).unwrap()));
            }
        })
    });
    group.finish();
}

fn mapping(c: &mut Criterion) {
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

fn outlines(c: &mut Criterion) {
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
    variable.set_variations(Some(900.0), None, None, false);
    group.bench_function("variable_256", |b| {
        b.iter(|| {
            for _ in 0..256 {
                black_box(variable.glyph_outline(black_box(1)));
            }
        })
    });
    group.finish();
}

fn pipeline(c: &mut Criterion) {
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

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = loading, mapping, outlines, pipeline
}
criterion_main!(benches);
