//! Render-time benchmark for a pattern-heavy page (> 10,000 tiles across
//! coloured and uncoloured tiling patterns, mirroring MOZILLA-1244729-0 and
//! PDFBOX-1094-5 without depending on external corpus files).
//!
//! Run:
//!   cargo bench -p zpdf-benches --bench tiling_pattern

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", content.len()).into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

/// 1200×900 pt page tiled with 6×6 pt coloured and uncoloured cells (30,000
/// tiles total across four distinct pattern regions).
fn build_pattern_heavy_pdf() -> Vec<u8> {
    let cell_grid: &[u8] = b"0.95 0.95 0.95 rg 0 0 6 6 re f\n\
                             0 0 0 RG 0.4 w 0 0 6 6 re S\n\
                             0.2 0.2 0.2 rg 1.5 1.5 3 3 re f";
    let cell_hatch: &[u8] = b"0 0.4 0.8 RG 0.5 w 0 0 m 6 6 l S 0 6 m 6 0 l S";
    let cell_stipple: &[u8] = b"1 1 1.5 1.5 re f 3.5 3.5 1.5 1.5 re f";

    let pat1 = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2 \
                /BBox [0 0 6 6] /XStep 6 /YStep -6 /Resources << >>";
    let pat2 = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                /BBox [0 0 6 6] /XStep 6 /YStep 6 /Resources << >>";
    let pat3 = "/Type /Pattern /PatternType 1 /PaintType 2 /TilingType 3 \
                /BBox [0 0 6.2 6.2] /XStep 6 /YStep 6 /Resources << >>";

    let content: &[u8] = b"/Pattern cs /P1 scn 0 0 600 450 re f\n\
                           /P2 scn 600 0 600 450 re f\n\
                           /CS0 cs 0.8 0.1 0.1 /P3 scn 0 450 600 450 re f\n\
                           0.1 0.6 0.2 /P3 scn 600 450 600 450 re f";

    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1200 900] /Contents 4 0 R \
          /Resources << /Pattern << /P1 5 0 R /P2 6 0 R /P3 7 0 R >> \
                        /ColorSpace << /CS0 8 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat1, cell_grid),
        stream_obj(pat2, cell_hatch),
        stream_obj(pat3, cell_stipple),
        b"[/Pattern /DeviceRGB]".to_vec(),
    ])
}

fn bench_tiling_pattern(c: &mut Criterion) {
    let pdf_bytes = build_pattern_heavy_pdf();
    let doc = PdfDocument::open(pdf_bytes).expect("open synthetic pattern PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content bytes");

    let mut group = c.benchmark_group("tiling_pattern_30k_tiles");
    group.sample_size(20);

    group.bench_function("interpret", |b| {
        b.iter(|| {
            let mut fonts = doc.load_page_fonts(&page);
            let mut images = ImageCache::new();
            let dl = ContentInterpreter::new(page.media_box)
                .with_fonts(&mut fonts)
                .with_document(doc.file(), &page.resources)
                .with_images(&mut images)
                .interpret(&content);
            black_box(dl.commands.len())
        })
    });

    let mut fonts = doc.load_page_fonts(&page);
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);

    for (label, dpi) in [
        ("render_cpu_72dpi", 72.0_f32),
        ("render_cpu_150dpi", 150.0_f32),
    ] {
        let scale = dpi / 72.0;
        let pixels = (1200.0 * scale) as u64 * (900.0 * scale) as u64;
        group.throughput(Throughput::Elements(pixels));
        group.bench_function(label, |b| {
            b.iter(|| {
                let rendered = zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&fonts)
                    .with_images(&images)
                    .render_display_list(&dl, scale)
                    .expect("render");
                black_box(rendered.data.len())
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_tiling_pattern);
criterion_main!(benches);
