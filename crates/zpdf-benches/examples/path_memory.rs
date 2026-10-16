//! Display-list path cost on line-heavy (CAD) and curve-dense pages: heap
//! peak, heap retained by the interpreted page, allocation count, and time to
//! interpret and CPU-render.
//!
//! cargo run -p zpdf-benches --release --example path_memory -- \
//!     [--dpi N] [--runs N] [file.pdf[:page] ...]
//!
//! Two synthetic pages are always measured; any PDFs given are measured too.
//! Heap numbers come from a counting global allocator and barely vary between
//! runs; times are the fastest of `--runs` runs. The `raster` column hashes
//! the rendered pixels, so two builds can be compared for identical output.
//! Results: `docs/performance/paths.md`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use zpdf::display_list::{DisplayList, Path, PathElement, RenderCommand};
use zpdf::{PdfDocument, RenderBackend};
use zpdf_benches::pipeline::{interpret_page, PageSetup};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

fn grew(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call forwards to `System` unchanged; only counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        new
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[derive(Clone, Copy)]
struct Usage {
    /// Highest live heap during the phase, above its starting point.
    peak: usize,
    /// Live heap the phase left behind (its output).
    retained: isize,
    /// `alloc` + `realloc` calls.
    allocations: u64,
    nanos: u128,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Usage) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    let out = f();
    let nanos = started.elapsed().as_nanos();
    let usage = Usage {
        peak: PEAK.load(Ordering::Relaxed) - base,
        retained: CURRENT.load(Ordering::Relaxed) as isize - base as isize,
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - allocations,
        nanos,
    };
    (out, usage)
}

/// Deterministic coordinates for the synthetic pages.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, low: f64, high: f64) -> f64 {
        low + self.next() * (high - low)
    }
}

/// An A1 landscape sheet in AutoCAD-style 0.1 pt units: 200k single
/// `m l S` segments, 2k 50-vertex polylines and 20k hatch rectangles.
fn cad_sheet() -> String {
    let mut rng = Lcg(1);
    let mut s = String::from("q 0.1 0 0 0.1 0 0 cm 1 J 1 j\n");
    for i in 0..200_000 {
        if i % 1000 == 0 {
            let _ = writeln!(s, "{:.1} w 0 0 {:.2} RG", rng.range(1.0, 5.0), rng.next());
        }
        let (x, y) = (rng.range(0.0, 23840.0), rng.range(0.0, 16840.0));
        let (dx, dy) = (rng.range(-500.0, 500.0), rng.range(-500.0, 500.0));
        let _ = writeln!(s, "{x:.1} {y:.1} m {:.1} {:.1} l S", x + dx, y + dy);
    }
    for _ in 0..2_000 {
        let (mut x, mut y) = (rng.range(0.0, 23840.0), rng.range(0.0, 16840.0));
        let _ = write!(s, "{x:.1} {y:.1} m");
        for _ in 0..49 {
            x += rng.range(-200.0, 200.0);
            y += rng.range(-200.0, 200.0);
            let _ = write!(s, " {x:.1} {y:.1} l");
        }
        s.push_str(" S\n");
    }
    for i in 0..20_000 {
        if i % 1000 == 0 {
            let _ = writeln!(s, "{:.2} 0 0 rg", rng.next());
        }
        let _ = writeln!(
            s,
            "{:.1} {:.1} {:.1} {:.1} re f",
            rng.range(0.0, 23840.0),
            rng.range(0.0, 16840.0),
            rng.range(5.0, 400.0),
            rng.range(5.0, 30.0)
        );
    }
    s.push_str("Q\n");
    s
}

/// An A1 landscape page of 50k four-curve circles (alternately stroked and
/// filled) and 10k open single-curve arcs.
fn curve_sheet() -> String {
    const K: f64 = 0.552_284_749_8;
    let mut rng = Lcg(2);
    let mut s = String::from("q 0.5 w\n");
    for i in 0..50_000 {
        let (cx, cy, r) = (
            rng.range(0.0, 2384.0),
            rng.range(0.0, 1684.0),
            rng.range(1.0, 40.0),
        );
        let k = r * K;
        let _ = writeln!(
            s,
            "{:.2} {cy:.2} m {:.2} {:.2} {:.2} {:.2} {cx:.2} {:.2} c \
             {:.2} {:.2} {:.2} {:.2} {:.2} {cy:.2} c \
             {:.2} {:.2} {:.2} {:.2} {cx:.2} {:.2} c \
             {:.2} {:.2} {:.2} {:.2} {:.2} {cy:.2} c h {}",
            cx + r,
            cx + r,
            cy + k,
            cx + k,
            cy + r,
            cy + r,
            cx - k,
            cy + r,
            cx - r,
            cy + k,
            cx - r,
            cx - r,
            cy - k,
            cx - k,
            cy - r,
            cy - r,
            cx + k,
            cy - r,
            cx + r,
            cy - k,
            cx + r,
            if i % 2 == 0 { "S" } else { "f" }
        );
    }
    for _ in 0..10_000 {
        let (x, y) = (rng.range(0.0, 2384.0), rng.range(0.0, 1684.0));
        let _ = writeln!(
            s,
            "{x:.2} {y:.2} m {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c S",
            x + rng.range(-30.0, 30.0),
            y + rng.range(-30.0, 30.0),
            x + rng.range(-30.0, 30.0),
            y + rng.range(-30.0, 30.0),
            x + rng.range(-60.0, 60.0),
            y + rng.range(-60.0, 60.0)
        );
    }
    s.push_str("Q\n");
    s
}

/// A one-page PDF around an uncompressed content stream.
fn single_page_pdf(width: f64, height: f64, content: &str) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] \
             /Resources << >> /Contents 4 0 R >>"
        ),
    ];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    offsets.push(pdf.len());
    pdf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    pdf.extend_from_slice(content.as_bytes());
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref = pdf.len();
    let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1);
    for offset in &offsets {
        let _ = writeln!(table, "{offset:010} 00000 n ");
    }
    let _ = write!(
        table,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len() + 1
    );
    pdf.extend_from_slice(table.as_bytes());
    pdf
}

fn elements(path: &Path) -> impl Iterator<Item = PathElement> + '_ {
    path.iter()
}

/// (paths, lines, curves, all elements) across the display list.
fn path_counts(dl: &DisplayList) -> (usize, usize, usize, usize) {
    let mut counts = (0, 0, 0, 0);
    for command in &dl.commands {
        let path = match command {
            RenderCommand::FillPath { path, .. }
            | RenderCommand::StrokePath { path, .. }
            | RenderCommand::PushClip { path, .. }
            | RenderCommand::PushClipStroke { path, .. } => path,
            _ => continue,
        };
        counts.0 += 1;
        for element in elements(path) {
            counts.3 += 1;
            match element {
                PathElement::LineTo(_) => counts.1 += 1,
                PathElement::CurveTo(..) => counts.2 += 1,
                _ => {}
            }
        }
    }
    counts
}

fn mib(bytes: impl Into<f64>) -> String {
    format!("{:.1}", bytes.into() / (1024.0 * 1024.0))
}

fn measure_page(label: &str, pdf: Vec<u8>, page_index: usize, dpi: f32, runs: usize) {
    let doc = match PdfDocument::open(pdf) {
        Ok(doc) => doc,
        Err(error) => return eprintln!("{label}: {error}"),
    };
    let page = match doc.page(page_index) {
        Ok(page) => page,
        Err(error) => return eprintln!("{label}: {error}"),
    };
    let mut interpret = None::<Usage>;
    let mut render = None::<Usage>;
    let mut counts = (0, 0, 0, 0);
    let mut raster = 0;
    for _ in 0..runs.max(1) {
        let (setup, usage) = measure(|| interpret_page(&doc, &page, page.effective_box(), dpi));
        let setup: PageSetup = match setup {
            Ok((setup, _)) => setup,
            Err(error) => return eprintln!("{label}: {error}"),
        };
        counts = path_counts(&setup.dl);
        let (rendered, render_usage) = measure(|| {
            zpdf::cpu::CpuRenderer::new()
                .with_fonts(&setup.font_cache)
                .with_images(&setup.image_cache)
                .render_display_list(&setup.dl, setup.scale)
        });
        match rendered {
            Ok(page) => raster = fnv1a(&page.data),
            Err(error) => return eprintln!("{label}: render: {error}"),
        }
        let fastest = |best: Option<Usage>, new: Usage| {
            Some(match best {
                Some(best) if best.nanos <= new.nanos => best,
                _ => new,
            })
        };
        interpret = fastest(interpret, usage);
        render = fastest(render, render_usage);
    }
    let (interpret, render) = (interpret.unwrap(), render.unwrap());
    println!(
        "| {label} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {} | {} | {:.1} | `{:016x}` |",
        counts.0,
        counts.3,
        counts.1,
        counts.2,
        mib(interpret.retained as f64),
        mib(interpret.peak as f64),
        interpret.allocations,
        interpret.nanos as f64 / 1e6,
        mib(render.peak as f64),
        render.allocations,
        render.nanos as f64 / 1e6,
        raster,
    );
}

/// Identity of the rendered pixels, to show both builds draw the same page.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

fn main() {
    let mut dpi = 72.0;
    let mut runs = 5;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dpi" => dpi = args.next().and_then(|v| v.parse().ok()).expect("--dpi N"),
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).expect("--runs N"),
            _ => files.push(arg),
        }
    }
    println!("dpi {dpi}, fastest of {runs} runs; MiB = 2^20 bytes");
    println!();
    println!(
        "| page | paths | elements | lines | curves | retained MiB | interpret peak MiB | \
         interpret allocs | interpret ms | render peak MiB | render allocs | render ms | raster |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|");
    measure_page(
        "synthetic CAD sheet",
        single_page_pdf(2384.0, 1684.0, &cad_sheet()),
        0,
        dpi,
        runs,
    );
    measure_page(
        "synthetic curves",
        single_page_pdf(2384.0, 1684.0, &curve_sheet()),
        0,
        dpi,
        runs,
    );
    for file in files {
        let (path, page) = match file.rsplit_once(':') {
            Some((path, page)) if page.parse::<usize>().is_ok() => {
                (path.to_string(), page.parse().unwrap())
            }
            _ => (file.clone(), 0),
        };
        let label = format!(
            "{} p{page}",
            std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone())
        );
        match std::fs::read(&path) {
            Ok(pdf) => measure_page(&label, pdf, page, dpi, runs),
            Err(error) => eprintln!("{path}: {error}"),
        }
    }
}
