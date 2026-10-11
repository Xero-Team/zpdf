//! Decompose the combined font-construction control without deleting it.
//! Input copy, parser work, empty metrics and destruction are separate costs.

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use zpdf_benches::font_workloads as fixtures;
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

const FONT_LEN: usize = fixtures::PRIMARY.len();

#[repr(align(64))]
struct Aligned([u8; FONT_LEN]);

#[repr(C, align(64))]
struct Unaligned {
    prefix: u8,
    bytes: [u8; FONT_LEN],
}

static ALIGNED: Aligned = Aligned(*include_bytes!(
    "../../zpdf-font/tests/fixtures/coverage-primary.ttf"
));
static UNALIGNED: Unaligned = Unaligned {
    prefix: 0xFF,
    bytes: *include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf"),
};

fn diagnostic(c: &mut Criterion) {
    assert_eq!(ALIGNED.0.as_slice(), fixtures::PRIMARY);
    assert_eq!(UNALIGNED.bytes.as_slice(), fixtures::PRIMARY);
    assert_eq!(ALIGNED.0.as_ptr() as usize % 64, 0);
    assert_eq!(UNALIGNED.bytes.as_ptr() as usize % 64, 1);
    eprintln!(
        "font diagnostics: original static alignment mod 64 = {}",
        fixtures::PRIMARY.as_ptr() as usize % 64
    );
    let heap = fixtures::PRIMARY.to_vec();
    let mut group = c.benchmark_group("font_diagnostics");
    for (name, source) in [
        ("copy_static", fixtures::PRIMARY),
        ("copy_heap", heap.as_slice()),
        ("copy_aligned", ALIGNED.0.as_slice()),
        ("copy_unaligned", UNALIGNED.bytes.as_slice()),
    ] {
        group.bench_function(name, |b| b.iter(|| black_box(black_box(source).to_vec())));
    }
    group.bench_function("construct_owned", |b| {
        b.iter_batched(
            || fixtures::PRIMARY.to_vec(),
            |bytes| {
                black_box(LoadedFont::new_with_data(
                    PdfFontType::TrueType,
                    "ZpdfCoveragePrimary".into(),
                    black_box(bytes),
                    CidWidths::new(1000.0),
                ))
            },
            // Keep one live font input: large batches would benchmark a different
            // cache working set. Input copying and result destruction are excluded.
            BatchSize::PerIteration,
        )
    });
    group.bench_function("parse_metrics", |b| {
        b.iter(|| {
            let face = ttf_parser::Face::parse(black_box(&heap), 0).unwrap();
            black_box((face.units_per_em(), face.ascender(), face.descender()))
        })
    });
    group.bench_function("empty_widths", |b| {
        b.iter(|| black_box(CidWidths::new(1000.0)))
    });
    group.bench_function("drop_loaded", |b| {
        b.iter_batched(
            fixtures::primary_font,
            |font| drop(black_box(font)),
            BatchSize::PerIteration,
        )
    });
    let type1 = include_bytes!("../../zpdf-font/tests/fixtures/coverage-type1.pfa");
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

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = diagnostic
}
criterion_main!(benches);
