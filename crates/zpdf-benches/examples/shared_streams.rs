//! Shared raw-stream storage: run this same harness against both revisions.
//! Heap measurements exclude the immutable input fixture, which remains alive.
//! Timing excludes counters and output hashing. Imported source copies are
//! created inside the measured operation so accidental file retention is visible.
//! Usage: `shared_streams CASE [ITERATIONS_PER_SAMPLE]` (11 samples per process).
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};
use zpdf::{ObjectId, ParseLimits, PdfDocument, PdfFile, PdfObject, RenderBackend};
use zpdf_writer::{rewrite_pdf, EncryptionConfig, IncrementalWriter, RewriteOptions};

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

fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn objects_hash(objects: &[PdfObject]) -> String {
    let mut hash = Sha256::new();
    for object in objects {
        if let PdfObject::Stream(stream) = object {
            hash.update((stream.data.len() as u64).to_le_bytes());
            hash.update(stream.data.as_ref());
        } else {
            hash.update(format!("{object:?}"));
        }
    }
    hex(&hash.finalize())
}

fn report<T>(
    name: &str,
    input: &[u8],
    iterations: usize,
    mut f: impl FnMut() -> T,
    signature: impl Fn(&T) -> String,
) {
    let iterations = std::env::args()
        .nth(2)
        .map_or(iterations, |n| n.parse().expect("iterations"));
    assert!((1..=1_000_000).contains(&iterations));
    drop(f());
    LIVE.store(0, Relaxed);
    PEAK.store(0, Relaxed);
    ALLOCS.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let output = f();
    ACTIVE.store(false, Relaxed);
    let peak = PEAK.load(Relaxed);
    let retained = LIVE.load(Relaxed);
    let allocations = ALLOCS.load(Relaxed);
    assert!(retained <= peak);
    let output_sha256 = signature(&output);
    drop(output);
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
        serde_json::json!({
            "case":name,"input_sha256":hash(input),"source_bytes":input.len(),
            "output_sha256":output_sha256,"peak_bytes":peak,"retained_bytes":retained,
            "allocations":allocations,"iterations":iterations,"samples_ns":samples,
            "object_size":std::mem::size_of::<PdfObject>(),"stream_size":std::mem::size_of::<zpdf::PdfStream>()
        })
    );
}

fn scalar_pdf(count: usize) -> Vec<u8> {
    let mut writer = IncrementalWriter::new(pdf(&[], "")).unwrap();
    let object = PdfObject::Array((0..32).map(PdfObject::Integer).collect());
    for _ in 0..count {
        writer.add_object(&object);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    out.into_inner()
}

fn import_fixture() -> Vec<u8> {
    let mut writer =
        IncrementalWriter::new(pdf(&[b"0 0 1 rg 10 10 20 20 re f".to_vec()], "")).unwrap();
    writer.add_stream(&zpdf::PdfDict::new(), &vec![b'X'; 4 * 1024 * 1024]);
    let mut out = std::io::Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    out.into_inner()
}

fn main() {
    let case = std::env::args().nth(1).expect("case");
    let count = match case.as_str() {
        "small-streams" | "tiny-streams" => 1024,
        "scalar-objects" => 512,
        _ => 1,
    };
    let input = match case.as_str() {
        "scalar-objects" => scalar_pdf(count),
        "small-streams" => pdf(&vec![vec![b'A'; 128]; count], ""),
        "tiny-streams" => pdf(&vec![vec![b'A'; 1]; count], ""),
        "imports" => import_fixture(),
        "pipeline" => pdf(&[b"0 0 1 rg 10 10 80 80 re f\n".repeat(32)], ""),
        "warm-read" => pdf(&[vec![b'A'; 64 * 1024]], ""),
        "encrypted" => {
            let plain = pdf(&[vec![b'A'; 2 * 1024 * 1024]], "");
            rewrite_pdf(
                &PdfFile::parse(plain).unwrap(),
                &RewriteOptions {
                    compress_uncompressed: false,
                    encrypt: Some(EncryptionConfig::rc4_128("user", "owner")),
                    ..Default::default()
                },
            )
            .unwrap()
        }
        "raw-large" | "cold-open" | "no-cache" | "recovery" => {
            let mut bytes = pdf(&[vec![b'A'; 8 * 1024 * 1024]], "");
            if case == "recovery" {
                let offset = bytes
                    .windows(b"startxref".len())
                    .rposition(|b| b == b"startxref")
                    .unwrap();
                bytes.truncate(offset);
                bytes.extend_from_slice(b"startxref\n0\n%%EOF\n");
            }
            bytes
        }
        _ => panic!("unknown case"),
    };
    let input: Arc<[u8]> = input.into();
    let limits = ParseLimits {
        max_object_cache_bytes: if case == "no-cache" {
            0
        } else {
            ParseLimits::default().max_object_cache_bytes
        },
        ..Default::default()
    };
    let parse = || {
        PdfFile::parse_with_password_and_limits(Arc::clone(&input), b"user", limits.clone())
            .unwrap()
    };
    if case == "imports" {
        let base = pdf(&[], "");
        report(
            &case,
            &input,
            1,
            || {
                let mut writer = IncrementalWriter::new(base.clone()).unwrap();
                for _ in 0..4 {
                    let source = PdfFile::parse(input.to_vec()).unwrap();
                    writer.append_document(&source).unwrap();
                }
                writer
            },
            |writer| {
                let mut out = std::io::Cursor::new(Vec::new());
                writer.write(&mut out).unwrap();
                hash(out.get_ref())
            },
        );
    } else if case == "pipeline" {
        report(
            &case,
            &input,
            4,
            || {
                let doc = PdfDocument::open(Arc::clone(&input)).unwrap();
                let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
                page.ensure_complete().unwrap();
                zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&page.fonts)
                    .with_images(&page.images)
                    .render_display_list(&page.display_list, 1.0)
                    .unwrap()
                    .data
            },
            |bytes| hash(bytes),
        );
    } else if case == "cold-open" {
        report(&case, &input, 32, parse, |file| {
            format!("{}", file.xref.len())
        });
    } else if case == "warm-read" {
        let file = parse();
        drop(file.resolve(ObjectId(4, 0)).unwrap());
        report(
            &case,
            &input,
            32,
            || {
                (0..32)
                    .map(|_| file.resolve(ObjectId(4, 0)).unwrap())
                    .collect::<Vec<_>>()
            },
            |objects| objects_hash(objects),
        );
    } else {
        report(
            &case,
            &input,
            if case == "encrypted" { 1 } else { 4 },
            || {
                let file = parse();
                let objects = (0..count)
                    .map(|i| file.resolve(ObjectId(i as u32 + 4, 0)).unwrap())
                    .collect::<Vec<_>>();
                (file, objects)
            },
            |(_, objects)| {
                if case == "encrypted" {
                    let stream = objects[0].as_stream().expect("encrypted stream fixture");
                    assert_eq!(stream.data.len(), 2 * 1024 * 1024);
                    assert!(stream.data.iter().all(|&b| b == b'A'));
                }
                objects_hash(objects)
            },
        );
    }
}
