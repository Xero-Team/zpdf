//! Synthetic buffer-copy controls. Run the same source on both revisions:
//! cargo run -p zpdf-benches --release --example buffer_memory -- [case]
//! Reports requested live heap (not RSS), allocations, output hashes and timing.
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use flate2::{write::ZlibEncoder, Compression};
use sha2::{Digest, Sha256};
use zpdf::{ObjectId, PdfDocument, RenderBackend};

struct Counting;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
fn grow(size: usize) {
    if ACTIVE.load(Relaxed) {
        ALLOCS.fetch_add(1, Relaxed);
        PEAK.fetch_max(LIVE.fetch_add(size, Relaxed) + size, Relaxed);
    }
}
// SAFETY: all pointers/layouts are forwarded unchanged to System. Counting is
// enabled only around a synchronous operation with no pre-existing allocations
// released, after document caches have been warmed.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if ACTIVE.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            if ACTIVE.load(Relaxed) {
                LIVE.fetch_sub(layout.size(), Relaxed);
            }
            grow(size);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn pdf(streams: &[Vec<u8>], extra: &str) -> Vec<u8> {
    let refs = (0..streams.len())
        .map(|i| format!("{} 0 R", i + 4))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 128 128] /Contents [{refs}] >>")
            .into_bytes(),
    ];
    for stream in streams {
        let mut object = format!("<< /Length {} {extra} >>\nstream\n", stream.len()).into_bytes();
        object.extend_from_slice(stream);
        object.extend_from_slice(b"\nendstream");
        objects.push(object);
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", i + 1).unwrap();
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    out
}

fn report(name: &str, input: &[u8], iterations: usize, mut f: impl FnMut() -> Vec<u8>) {
    drop(f()); // Populate document caches before measuring transient buffers.
    LIVE.store(0, Relaxed);
    PEAK.store(0, Relaxed);
    ALLOCS.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let out = f();
    ACTIVE.store(false, Relaxed);
    let (peak, retained, allocations) =
        (PEAK.load(Relaxed), LIVE.load(Relaxed), ALLOCS.load(Relaxed));
    let output_sha256 = hash(&out);
    let output_bytes = out.len();
    drop(out);
    let mut samples = Vec::new();
    for _ in 0..11 {
        let start = Instant::now();
        for _ in 0..iterations {
            drop(black_box(f()));
        }
        samples.push(start.elapsed().as_nanos() / iterations as u128);
    }
    println!(
        "{}",
        serde_json::json!({"case":name,"input_sha256":hash(input),"output_sha256":output_sha256,"output_bytes":output_bytes,"peak_bytes":peak,"retained_bytes":retained,"allocations":allocations,"iterations":iterations,"samples_ns":samples})
    );
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn main() {
    let case = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "content-single".into());
    match case.as_str() {
        "content-single" | "content-multi" | "content-small" | "pipeline" => {
            let streams = match case.as_str() {
                "content-single" => vec![vec![b' '; 8 * 1024 * 1024]],
                "content-multi" => vec![vec![b' '; 4 * 1024 * 1024]; 2],
                "content-small" => vec![b"0 0 1 rg 10 10 80 80 re f".to_vec()],
                _ => vec![b"0 0 1 rg 10 10 80 80 re f\n".repeat(32)],
            };
            let input = pdf(&streams, "");
            let doc = PdfDocument::open(input.clone()).unwrap();
            let page = doc.page(0).unwrap();
            if case == "pipeline" {
                report(&case, &input, 8, || {
                    let p = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
                    p.ensure_complete().unwrap();
                    zpdf::cpu::CpuRenderer::new()
                        .with_fonts(&p.fonts)
                        .with_images(&p.images)
                        .render_display_list(&p.display_list, 1.0)
                        .unwrap()
                        .data
                });
            } else {
                report(
                    &case,
                    &input,
                    if case == "content-small" { 1024 } else { 8 },
                    || doc.page_content_bytes(&page).unwrap(),
                );
            }
        }
        "tiff" | "tiff-tail" | "tiff-small" | "flate" | "png" => {
            let predictor = match case.as_str() {
                "tiff" | "tiff-tail" | "tiff-small" => 2,
                "png" => 12,
                _ => 1,
            };
            let samples = match case.as_str() {
                "png" => vec![0; 4097 * 2048],
                "tiff-tail" => vec![1; 8 * 1024 * 1024 + 1],
                "tiff-small" => vec![1; 37],
                _ => vec![1; 8 * 1024 * 1024],
            };
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&samples).unwrap();
            let input = pdf(&[encoder.finish().unwrap()], &format!("/Filter /FlateDecode /DecodeParms << /Predictor {predictor} /Columns 4096 /Colors 1 >>"));
            let doc = PdfDocument::open(input.clone()).unwrap();
            report(
                &case,
                &input,
                if case == "tiff-small" { 1024 } else { 4 },
                || doc.file().resolve_stream_data(ObjectId(4, 0)).unwrap(),
            );
        }
        _ => panic!("unknown case: {case}"),
    }
}
