//! Costs of explicit memory-font registration and document integration.
//! The original `fonts` target remains unchanged as a regression control.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use zpdf::{FontProvider, PdfDocument};
use zpdf_benches::font_workloads as fixtures;
use zpdf_font::system::SubstituteHints;

fn supplied_pdf() -> Arc<[u8]> {
    let mut content = String::new();
    for row in 0..16 {
        content.push_str(&format!(
            "BT /F1 10 Tf 20 {} Td (AAAAAAAAAAAAAAAA) Tj ET\n",
            260 - row * 15
        ));
    }
    // The high-level writer deliberately restricts unembedded fonts to the
    // standard fourteen, so assemble this explicit nonembedded font fixture.
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 280] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
        "<< /Type /Font /Subtype /TrueType /BaseFont /ApplicationFace /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 65 /Widths [1000] /FontDescriptor 6 0 R >>".into(),
        "<< /Type /FontDescriptor /FontName /ApplicationFace /Flags 32 /FontBBox [0 -200 1000 900] /Ascent 900 /Descent -200 /CapHeight 800 /StemV 80 >>".into(),
    ];
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let start = bytes.len();
    bytes.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n").as_bytes(),
    );
    Arc::from(bytes)
}

fn provider() -> Arc<FontProvider> {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(fixtures::PRIMARY, 0, &["ApplicationFace"])
        .unwrap();
    provider
}

fn registration(c: &mut Criterion) {
    let bytes: Arc<[u8]> = Arc::from(fixtures::PRIMARY);
    let provider = provider();
    let id = provider.register(Arc::clone(&bytes), 0).unwrap();
    assert_eq!(provider.len(), 1);
    let mut group = c.benchmark_group("font_provider/registration");
    group.bench_function("cold", |b| {
        b.iter(|| {
            let provider = FontProvider::memory_only();
            black_box(provider.register(Arc::clone(&bytes), 0).unwrap());
        })
    });
    group.bench_function("duplicate", |b| {
        b.iter(|| {
            black_box(provider.register(Arc::clone(&bytes), 0).unwrap());
        })
    });
    assert_eq!(provider.register(Arc::clone(&bytes), 0).unwrap(), id);
    group.bench_function("name_hit", |b| {
        b.iter(|| {
            black_box(provider.find_font(
                black_box("ApplicationFace"),
                SubstituteHints::default(),
                None,
            ));
        })
    });
    assert!(provider
        .find_font("ApplicationFace", SubstituteHints::default(), None)
        .is_some());
    group.bench_function("name_miss", |b| {
        b.iter(|| {
            black_box(provider.find_font(
                black_box("UnregisteredFace"),
                SubstituteHints::default(),
                None,
            ));
        })
    });
    assert!(provider
        .find_font("UnregisteredFace", SubstituteHints::default(), None)
        .is_none());
    group.finish();
}

fn integration(c: &mut Criterion) {
    let provider = provider();
    let pdf = supplied_pdf();
    let doc = PdfDocument::open(Arc::clone(&pdf))
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    fixtures::assert_text_case(&doc, &"A".repeat(256), 150.0);
    let mut group = c.benchmark_group("font_provider/document");
    group.bench_function("registered_warm", |b| {
        b.iter(|| black_box(fixtures::setup(&doc, 150.0)))
    });
    group.bench_function("registered_cold", |b| {
        b.iter_batched(
            || {
                PdfDocument::open(Arc::clone(&pdf))
                    .unwrap()
                    .with_font_provider(Arc::clone(&provider))
            },
            |doc| black_box(fixtures::setup(&doc, 150.0)),
            BatchSize::SmallInput,
        )
    });
    let embedded = PdfDocument::open(fixtures::make_pdf(&"A".repeat(256), false, 1))
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    fixtures::assert_text_case(&embedded, &"A".repeat(256), 150.0);
    group.bench_function("empty_provider_embedded", |b| {
        b.iter(|| black_box(fixtures::setup(&embedded, 150.0)))
    });
    group.bench_function("late_registration", |b| {
        b.iter_batched(
            || {
                let provider = Arc::new(FontProvider::memory_only());
                let doc = PdfDocument::open(Arc::clone(&pdf))
                    .unwrap()
                    .with_font_provider(Arc::clone(&provider));
                black_box(doc.load_page_fonts(&doc.page(0).unwrap()));
                (doc, provider)
            },
            |(doc, provider)| {
                provider
                    .register_with_aliases(fixtures::PRIMARY, 0, &["ApplicationFace"])
                    .unwrap();
                black_box(fixtures::setup(&doc, 150.0))
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = registration, integration
}
criterion_main!(benches);
