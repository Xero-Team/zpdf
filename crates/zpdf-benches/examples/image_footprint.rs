//! Heap and wall time of a CPU page render with and without
//! `ContentInterpreter::with_image_downscale`, on synthetic pages whose large
//! images are drawn far smaller than their sample size. Both modes must
//! produce the same pixels; the example aborts otherwise.
//!
//! cargo run -p zpdf-benches --release --example image_footprint

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};
use zpdf::cpu::CpuRenderer;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

/// Counts live heap bytes and their high-water mark.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grow(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Concatenate 1-based objects with a classic xref table + trailer.
fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
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

/// A Flate RGB scan-like image: smooth shading with fine line work.
fn scan(width: usize, height: usize, seed: usize) -> Vec<u8> {
    let mut samples = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            let line = (x + seed) % 97 < 2 || (y + seed) % 89 < 2;
            let base = (
                (x * 255 / width) as u8,
                (y * 255 / height) as u8,
                (seed * 40) as u8,
            );
            if line {
                samples.extend_from_slice(&[20, 20, 20]);
            } else {
                samples.extend_from_slice(&[base.0, base.1, base.2]);
            }
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&samples).unwrap();
    encoder.finish().unwrap()
}

/// A letter page drawing `images` (width, height) with the given placements.
fn page(images: &[(usize, usize)], placements: &[(usize, [f64; 6])]) -> Vec<u8> {
    let names: String = (0..images.len())
        .map(|i| format!("/Im{i} {} 0 R ", 5 + i))
        .collect();
    let content: String = placements
        .iter()
        .map(|(index, [a, b, c, d, e, f])| {
            format!("q {a} {b} {c} {d} {e} {f} cm /Im{index} Do Q\n")
        })
        .collect();
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /XObject << {names}>> >> >>"
        )
        .into_bytes(),
        stream_obj("", content.as_bytes()),
    ];
    for (i, &(width, height)) in images.iter().enumerate() {
        objs.push(stream_obj(
            &format!(
                "/Type /XObject /Subtype /Image /Width {width} /Height {height} \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode"
            ),
            &scan(width, height, i),
        ));
    }
    assemble(&objs)
}

struct Measured {
    peak: usize,
    retained: u64,
    millis: f64,
    digest: [u8; 32],
}

fn measure(pdf: &[u8], dpi: f32, downscale: bool) -> Measured {
    let doc = PdfDocument::open(pdf.to_vec()).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.page_content_bytes(&page).unwrap();
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let started = Instant::now();
    let scale = dpi / 72.0;
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = ImageCache::new();
    let mut interpreter = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images);
    if downscale {
        let raster_scale = CpuRenderer::new()
            .with_limits(doc.file().limits())
            .raster_scale(page.effective_box(), scale)
            .unwrap();
        interpreter = interpreter.with_image_downscale(raster_scale);
    }
    let dl = interpreter.interpret(&content);
    let retained = images.bytes_used();
    let rendered = CpuRenderer::new()
        .with_limits(doc.file().limits())
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, scale)
        .unwrap();
    let millis = started.elapsed().as_secs_f64() * 1000.0;
    let peak = PEAK.load(Ordering::Relaxed) - base;
    Measured {
        peak,
        retained,
        millis,
        digest: Sha256::digest(&rendered.data).into(),
    }
}

fn mib(bytes: impl Into<u64>) -> f64 {
    bytes.into() as f64 / (1024.0 * 1024.0)
}

fn main() {
    let a0_scan = (6000, 8000);
    let cases = [
        (
            "one 6000x8000 scan at 300x400 pt",
            page(&[a0_scan], &[(0, [300.0, 0.0, 0.0, 400.0, 150.0, 200.0])]),
        ),
        (
            "same scan at two sizes",
            page(
                &[a0_scan],
                &[
                    (0, [300.0, 0.0, 0.0, 400.0, 20.0, 20.0]),
                    (0, [150.0, 0.0, 0.0, 200.0, 400.0, 500.0]),
                ],
            ),
        ),
        (
            "eight 3000x3000 tiles at 140x140 pt",
            page(
                &[(3000, 3000); 8],
                &(0..8)
                    .map(|i| {
                        let (x, y) = (
                            (i % 4) as f64 * 145.0 + 16.0,
                            (i / 4) as f64 * 145.0 + 250.0,
                        );
                        (i, [140.0, 0.0, 0.0, 140.0, x, y])
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
    ];
    println!("| page | dpi | peak heap off → on (MiB) | retained images off → on (MiB) | wall off → on (ms) |");
    println!("|---|---|---|---|---|");
    for (name, pdf) in &cases {
        for dpi in [72.0, 150.0] {
            // Median of three, alternating modes.
            let mut runs: [Vec<Measured>; 2] = [Vec::new(), Vec::new()];
            for _ in 0..3 {
                for (slot, downscale) in [false, true].into_iter().enumerate() {
                    runs[slot].push(measure(pdf, dpi, downscale));
                }
            }
            for mode in &mut runs {
                mode.sort_by(|a, b| a.millis.total_cmp(&b.millis));
            }
            let (off, on) = (&runs[0][1], &runs[1][1]);
            assert!(
                runs.iter().flatten().all(|run| run.digest == off.digest),
                "{name} at {dpi} dpi: rasters differ"
            );
            println!(
                "| {name} | {dpi} | {:.0} → {:.0} | {:.1} → {:.1} | {:.0} → {:.0} |",
                mib(off.peak as u64),
                mib(on.peak as u64),
                mib(off.retained),
                mib(on.retained),
                off.millis,
                on.millis,
            );
        }
    }
}
