//! Read-allocation comparison; the identical harness builds on main and this PR.
//! Run in an isolated environment: read_memory <case> <iterations>.
//! Heap counts exclude immutable fixtures; Linux RSS includes the whole process.
use flate2::{write::ZlibEncoder, Compression};
use sha2::{Digest, Sha256};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    Arc,
};
use std::time::Instant;
use zpdf::{ObjectId, PdfDocument, PdfFile, PdfObject, RenderBackend};

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

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn rss_kib(key: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

fn report<T>(
    name: &str,
    input: &[u8],
    iterations: usize,
    mut f: impl FnMut() -> T,
    observe: impl Fn(&T) -> Vec<u8>,
) {
    drop(f());
    LIVE.store(0, Relaxed);
    PEAK.store(0, Relaxed);
    ALLOCS.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let out = f();
    ACTIVE.store(false, Relaxed);
    let (peak, retained, allocations) =
        (PEAK.load(Relaxed), LIVE.load(Relaxed), ALLOCS.load(Relaxed));
    let rss = rss_kib("VmRSS:");
    let rss_peak = rss_kib("VmHWM:");
    let bytes = observe(&out);
    let output_sha256 = hash(&bytes);
    let output_bytes = bytes.len();
    drop(bytes);
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
        serde_json::json!({"case":name,"input_sha256":hash(input),
        "output_sha256":output_sha256,"output_bytes":output_bytes,
        "peak_bytes":peak,"retained_bytes":retained,"allocations":allocations,
        "rss_kib":rss,"rss_peak_kib":rss_peak,"iterations":iterations,"samples_ns":samples})
    );
}

fn stream(data: &[u8], extra: &str) -> Vec<u8> {
    let mut out = format!("<< /Length {} {extra} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", i + 1).unwrap();
        out.extend_from_slice(obj);
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

fn page_objects(content: &[u8], resources: &str) -> Vec<Vec<u8>> {
    vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 256 256] /Contents 4 0 R /Resources << {resources} >> >>").into_bytes(),
        stream(content, "")]
}

// On main this adapter uses the old owned API. On this PR the inherent method
// takes precedence. This is the ONLY variant-dependent operation in the source.
#[allow(dead_code)]
trait SharedDecodeBaseline {
    fn resolve_stream_data_shared(&self, id: ObjectId) -> zpdf::Result<Vec<u8>>;
}
impl SharedDecodeBaseline for PdfFile {
    fn resolve_stream_data_shared(&self, id: ObjectId) -> zpdf::Result<Vec<u8>> {
        self.resolve_stream_data(id)
    }
}

fn render_pixels(page: &zpdf::InterpretedPage) -> Vec<u8> {
    page.ensure_complete().unwrap();
    zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page.fonts)
        .with_images(&page.images)
        .render_display_list(&page.display_list, 1.)
        .unwrap()
        .data
}

fn observe_page(page: &zpdf::InterpretedPage) -> Vec<u8> {
    let mut bytes = render_pixels(page);
    bytes.extend_from_slice(&page.stats.operators_executed.to_le_bytes());
    bytes
}

fn main() {
    let case = std::env::args().nth(1).expect("case");
    let iterations: usize = std::env::args()
        .nth(2)
        .expect("iterations")
        .parse()
        .unwrap();
    assert!(iterations > 0);
    match case.as_str() {
        "png-up" | "png-paeth" | "png-tail" | "flate" => {
            let mut samples = vec![0; 4097 * 1024];
            for (i, row) in samples.chunks_mut(4097).enumerate() {
                row[0] = if case == "png-paeth" { 4 } else { 2 };
                for (x, v) in row[1..].iter_mut().enumerate() {
                    *v = (i * 13 + x * 37) as u8;
                }
            }
            if case == "png-tail" {
                samples.truncate(samples.len() - 137);
            }
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
            enc.write_all(&samples).unwrap();
            let extra = if case == "flate" {
                "/Filter /FlateDecode"
            } else {
                "/Filter /FlateDecode /DecodeParms << /Predictor 15 /Columns 4096 /Colors 1 >>"
            };
            let input = pdf(&[stream(&enc.finish().unwrap(), extra)]);
            let file = PdfFile::parse(input.as_slice()).unwrap();
            report(
                &case,
                &input,
                iterations,
                || file.resolve_stream_data(ObjectId(1, 0)).unwrap(),
                |x| x.clone(),
            );
        }
        "raw-shared" | "raw-small" | "jpx-shared" | "decode-owned" => {
            let size = if case == "raw-small" {
                37
            } else {
                8 * 1024 * 1024
            };
            let input = pdf(&[stream(
                &vec![b'X'; size],
                if case == "jpx-shared" {
                    "/Filter /JPXDecode"
                } else {
                    ""
                },
            )]);
            let file = PdfFile::parse(input.as_slice()).unwrap();
            if case == "decode-owned" {
                report(
                    &case,
                    &input,
                    iterations,
                    || file.resolve_stream_data(ObjectId(1, 0)).unwrap(),
                    |x| x.clone(),
                );
            } else {
                report(
                    &case,
                    &input,
                    iterations,
                    || file.resolve_stream_data_shared(ObjectId(1, 0)).unwrap(),
                    |x| AsRef::<[u8]>::as_ref(x).to_vec(),
                );
            }
        }
        "shared-cold" | "shared-uncached" | "shared-warm" | "owned-warm" => {
            let objects: Vec<_> = (0..32)
                .map(|_| {
                    format!(
                        "<< /Payload ({}) /Nested [1 2 << /Key /Value >>] >>",
                        "abc".repeat(1024)
                    )
                    .into_bytes()
                })
                .collect();
            let input: Arc<[u8]> = pdf(&objects).into();
            let ids: Vec<_> = (1..=32).map(|i| ObjectId(i, 0)).collect();
            let file = PdfFile::parse(Arc::clone(&input)).unwrap();
            for &id in &ids {
                file.resolve_shared(id).unwrap();
            }
            let observe = |v: &Vec<Arc<PdfObject>>| format!("{v:?}").into_bytes();
            if case == "shared-cold" || case == "shared-uncached" {
                report(
                    &case,
                    &input,
                    iterations,
                    || {
                        let mut limits = zpdf::ParseLimits::default();
                        if case == "shared-uncached" {
                            limits.max_object_cache_bytes = 0;
                        }
                        let file = PdfFile::parse_with_limits(Arc::clone(&input), limits).unwrap();
                        ids.iter()
                            .map(|&id| file.resolve_shared(id).unwrap())
                            .collect()
                    },
                    observe,
                );
            } else if case == "shared-warm" {
                report(
                    &case,
                    &input,
                    iterations,
                    || {
                        ids.iter()
                            .map(|&id| file.resolve_shared(id).unwrap())
                            .collect()
                    },
                    observe,
                );
            } else {
                report(
                    &case,
                    &input,
                    iterations,
                    || {
                        ids.iter()
                            .map(|&id| file.resolve(id).unwrap())
                            .collect::<Vec<_>>()
                    },
                    |v| format!("{v:?}").into_bytes(),
                );
            }
        }
        "operators" | "resources" | "scan-rgb" | "scan-gray" | "scan-native" | "pipeline"
        | "pipeline-interpret" | "pipeline-render" => {
            let objects = if case.starts_with("scan-") {
                let gray = case == "scan-gray";
                let n = if gray { 1 } else { 3 };
                let data: Vec<_> = (0..2048 * 1536 * n).map(|i| (i * 37 + 19) as u8).collect();
                let mut objects =
                    page_objects(b"q 192 0 0 144 0 0 cm /Im Do Q", "/XObject << /Im 5 0 R >>");
                objects.push(stream(&data, &format!("/Type /XObject /Subtype /Image /Width 2048 /Height 1536 /BitsPerComponent 8 /ColorSpace /{}",if gray {"DeviceGray"} else {"DeviceRGB"})));
                objects
            } else if case == "resources" {
                let mut objects = page_objects(
                    &b"q /Gs gs /Fm Do Q\n".repeat(256),
                    "/XObject << /Fm 5 0 R >> /ExtGState << /Gs 6 0 R >>",
                );
                objects.push(stream(
                    b"0 g 1 1 12 12 re f",
                    &format!(
                        "/Subtype /Form /BBox [0 0 256 256] /Unused ({})",
                        "x".repeat(8192)
                    ),
                ));
                objects.push(
                    format!(
                        "<< /Type /ExtGState /LW 1 /Unused ({}) >>",
                        "y".repeat(8192)
                    )
                    .into_bytes(),
                );
                objects
            } else {
                let content = if case == "operators" {
                    b"q 0 g Q\n".repeat(4096)
                } else {
                    b"0 0 1 rg 10 10 80 80 re f\n".repeat(32)
                };
                page_objects(&content, "")
            };
            let input = pdf(&objects);
            let doc = PdfDocument::open(input.as_slice()).unwrap();
            let mut options = zpdf::PageInterpretOptions::default();
            if case.starts_with("scan-") && case != "scan-native" {
                options.image_downscale = Some(1.);
            }
            if case == "pipeline" {
                report(
                    &case,
                    &input,
                    iterations,
                    || {
                        let page = zpdf::interpret_page(&doc, 0, options).unwrap();
                        (render_pixels(&page), page.stats.operators_executed)
                    },
                    |(pixels, operators)| {
                        // Verification metadata belongs outside timing/heap
                        // accounting: appending to the RGBA Vec can reallocate
                        // and double its capacity, which is not rendering work.
                        let mut bytes = pixels.clone();
                        bytes.extend_from_slice(&operators.to_le_bytes());
                        bytes
                    },
                );
            } else if case == "pipeline-render" {
                let page = zpdf::interpret_page(&doc, 0, options).unwrap();
                report(
                    &case,
                    &input,
                    iterations,
                    || render_pixels(&page),
                    |pixels| {
                        let mut bytes = pixels.clone();
                        bytes.extend_from_slice(&page.stats.operators_executed.to_le_bytes());
                        bytes
                    },
                );
            } else {
                report(
                    &case,
                    &input,
                    iterations,
                    || zpdf::interpret_page(&doc, 0, options).unwrap(),
                    observe_page,
                );
            }
        }
        _ => panic!("unknown case {case}"),
    }
}
