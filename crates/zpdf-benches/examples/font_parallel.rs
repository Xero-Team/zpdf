//! Parallel page interpretation with one shared `FontProvider` versus one
//! provider per thread. A gap between the two at equal thread counts is time
//! spent waiting on provider-wide state rather than doing font work.
//!
//! cargo run -p zpdf-benches --release --example font_parallel -- [seconds]

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use zpdf::{ContentInterpreter, FontProvider, PdfDocument};
use zpdf_benches::{font_fallback_workloads, font_workloads};

struct Workload {
    name: &'static str,
    pdf: Arc<[u8]>,
    provider: fn() -> Arc<FontProvider>,
}

fn embedded_provider() -> Arc<FontProvider> {
    // One registered face puts every text operation on the provider path even
    // though the embedded font draws every glyph itself.
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(font_workloads::FALLBACK, 0).unwrap();
    provider
}

/// Pages interpreted per second by `threads` workers during `window`.
fn run(workload: &Workload, threads: usize, shared: bool, window: Duration) -> f64 {
    let shared_provider = (workload.provider)();
    let barrier = Arc::new(Barrier::new(threads + 1));
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let provider = if shared {
                Arc::clone(&shared_provider)
            } else {
                (workload.provider)()
            };
            let pdf = Arc::clone(&workload.pdf);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let doc = PdfDocument::open(pdf.to_vec())
                    .unwrap()
                    .with_font_provider(provider);
                let page = doc.page(0).unwrap();
                let content = doc.page_content_bytes(&page).unwrap();
                let interpret = || {
                    let mut fonts = doc.load_page_fonts(&page);
                    let dl = ContentInterpreter::new(page.effective_box())
                        .with_fonts(&mut fonts)
                        .interpret(&content);
                    std::hint::black_box(dl.commands.len());
                };
                // Warm the provider's decisions before the timed window.
                for _ in 0..20 {
                    interpret();
                }
                barrier.wait();
                let started = Instant::now();
                let mut pages = 0u64;
                while started.elapsed() < window {
                    interpret();
                    pages += 1;
                }
                pages as f64 / started.elapsed().as_secs_f64()
            })
        })
        .collect();
    barrier.wait();
    workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .sum()
}

fn main() {
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(2.0);
    let window = Duration::from_secs_f64(seconds);
    let cores = std::thread::available_parallelism().map_or(4, |count| count.get());
    let mut counts = vec![1];
    while counts.last().unwrap() * 2 <= cores {
        counts.push(counts.last().unwrap() * 2);
    }
    if *counts.last().unwrap() != cores {
        counts.push(cores);
    }
    let (_, cjk, _) = font_workloads::text_cases().swap_remove(1);
    let workloads = [
        Workload {
            name: "embedded_cjk",
            pdf: font_workloads::make_pdf(&cjk, true, 1),
            provider: embedded_provider,
        },
        Workload {
            name: "mixed_fallback",
            pdf: font_fallback_workloads::pdf(false, 0, false, 16),
            provider: font_fallback_workloads::provider,
        },
    ];
    println!("cores={cores} window={seconds}s");
    println!("workload        threads  shared/s  private/s  shared/private  shared scaling");
    for workload in &workloads {
        let base = run(workload, 1, true, window);
        for &threads in &counts {
            let shared = run(workload, threads, true, window);
            let private = run(workload, threads, false, window);
            println!(
                "{:<15} {:>7} {:>9.0} {:>10.0} {:>15.2} {:>15.2}",
                workload.name,
                threads,
                shared,
                private,
                shared / private,
                shared / base
            );
        }
    }
}
