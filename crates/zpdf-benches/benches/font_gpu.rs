//! Portable GPU font controls. Record the actual adapter and validate CPU ink
//! before timing; no software-adapter skip can masquerade as a passing run.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::time::Duration;
use zpdf::RenderBackend;
use zpdf_benches::{font_fallback_workloads, font_workloads, PageSetup};

fn validate(setup: &PageSetup, renderer: &mut zpdf::gpu::WgpuRenderer<'_>) {
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    let gpu = renderer
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let (mut cpu_ink, mut gpu_ink, mut overlap, mut union) = (0usize, 0usize, 0usize, 0usize);
    for (cpu, gpu) in cpu
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(gpu.data.as_chunks::<4>().0)
    {
        let c = cpu[0] < 128;
        let g = gpu[0] < 128;
        cpu_ink += usize::from(c);
        gpu_ink += usize::from(g);
        overlap += usize::from(c && g);
        union += usize::from(c || g);
    }
    assert!(cpu_ink > 100 && gpu_ink > 100, "blank GPU benchmark");
    assert!(
        overlap * 100 >= union * 70,
        "GPU placement differs from CPU"
    );
    assert!(
        gpu_ink * 4 >= cpu_ink * 3 && gpu_ink * 4 <= cpu_ink * 5,
        "GPU dropped or added substantial ink"
    );
}

fn gpu_fonts(c: &mut Criterion) {
    let cases = font_workloads::text_cases();
    let mut setups = Vec::new();
    for (name, text, composite) in cases {
        let doc = zpdf::PdfDocument::open(font_workloads::make_pdf(&text, composite, 1)).unwrap();
        font_workloads::assert_text_case(&doc, &text, 144.0);
        setups.push((name, font_workloads::setup(&doc, 144.0)));
    }
    let vertical = font_fallback_workloads::document(true, 0, false, 8);
    setups.push(("vertical_mixed", font_workloads::setup(&vertical, 144.0)));
    let repeated_pdf = font_workloads::make_pdf(&"A".repeat(256), false, 1);
    let doc = zpdf::PdfDocument::open(repeated_pdf).unwrap();
    let mut shear = font_workloads::setup(&doc, 144.0);
    let mut mirror = font_workloads::setup(&doc, 144.0);
    for (setup, mirrored) in [(&mut shear, false), (&mut mirror, true)] {
        for command in &mut setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                if mirrored {
                    run.h_scale = -1.0;
                } else {
                    run.transform.c = 0.2;
                }
            }
        }
    }
    setups.push(("sheared", shear));
    setups.push(("mirrored", mirror));
    let mut identity = zpdf::gpu::WgpuRenderer::new();
    let adapter = identity
        .adapter_identity()
        .expect("real GPU adapter required");
    let metadata = serde_json::json!({"fingerprint": adapter.fingerprint(), "description": adapter.describe()});
    eprintln!("font GPU adapter: {}", adapter.describe());
    if let Some(path) = std::env::var_os("ZPDF_BENCH_GPU_METADATA") {
        std::fs::write(path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
    }
    let mut group = c.benchmark_group("font_gpu/render_readback");
    for (name, setup) in &setups {
        let mut renderer = zpdf::gpu::WgpuRenderer::new().with_fonts(&setup.font_cache);
        validate(setup, &mut renderer);
        group.bench_function(*name, |b| {
            b.iter(|| {
                black_box(
                    renderer
                        .render_display_list(&setup.dl, setup.scale)
                        .unwrap(),
                )
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = gpu_fonts
}
criterion_main!(benches);
