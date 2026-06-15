use std::env;
use std::fs;
use std::process;

fn main() {
    // RUST_LOG-controlled diagnostics (e.g. RUST_LOG=zpdf_font=warn). Defaults to
    // silent so normal CLI output stays clean.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: zpdf <command> [args...]");
        eprintln!("Commands: info, dump, render, text, compare, debug-stream");
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "info" => cmd_info(&args[2..]),
        "dump" => cmd_dump(&args[2..]),
        "render" => cmd_render(&args[2..]),
        "text" => cmd_text(&args[2..]),
        "compare" => cmd_compare(&args[2..]),
        "debug-stream" => cmd_debug_stream(&args[2..]),
        other => {
            eprintln!("Unknown command: {other}");
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn cmd_info(args: &[String]) -> zpdf::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: zpdf info <file.pdf>");
        process::exit(1);
    }

    let data = fs::read(&args[0]).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;
    let (major, minor) = doc.version();

    println!("File: {}", args[0]);
    println!("Version: PDF-{major}.{minor}");
    println!("Pages: {}", doc.page_count());

    // Cap the per-page listing: a fuzzed/huge document can carry hundreds of
    // thousands of pages, and resolving + printing each (a fresh inheritance
    // walk per page) would make `info` appear to hang. The first N is enough to
    // characterize the file.
    const MAX_LISTED_PAGES: usize = 1000;
    let listed = doc.page_count().min(MAX_LISTED_PAGES);
    for i in 0..listed {
        if let Ok(page) = doc.page(i) {
            println!(
                "  Page {}: {:.0} x {:.0} pt (rotate: {})",
                i + 1,
                page.width(),
                page.height(),
                page.rotate,
            );
        }
    }
    if doc.page_count() > listed {
        println!("  ... and {} more pages", doc.page_count() - listed);
    }

    Ok(())
}

fn cmd_dump(args: &[String]) -> zpdf::Result<()> {
    if args.len() < 3 {
        eprintln!("Usage: zpdf dump <file.pdf> <obj_num> <gen_num>");
        process::exit(1);
    }

    let data = fs::read(&args[0]).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;

    let obj_num: u32 = args[1].parse().unwrap_or(0);
    let gen_num: u16 = args[2].parse().unwrap_or(0);
    let id = zpdf::ObjectId(obj_num, gen_num);

    let obj = doc.file().resolve(id)?;
    println!("{obj}");

    Ok(())
}

/// Pixel-compare two PNGs and report difference metrics. Serves as the
/// CPU↔reference (and, in Phase 3, GPU↔CPU) rendering comparison harness.
fn cmd_compare(args: &[String]) -> zpdf::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: zpdf compare <a.png> <b.png> [--out <diff.png>] [--threshold <0-255>]");
        process::exit(1);
    }
    let a_path = &args[0];
    let b_path = &args[1];
    let mut out_path: Option<String> = None;
    let mut threshold: u8 = 16;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out_path = args.get(i).cloned();
            }
            "--threshold" => {
                i += 1;
                threshold = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(16);
            }
            _ => {}
        }
        i += 1;
    }

    let load = |p: &str| {
        image::open(p)
            .map(|img| img.to_rgba8())
            .map_err(|e| zpdf::Error::StreamDecode(format!("open {p}: {e}")))
    };
    let a = load(a_path)?;
    let b = load(b_path)?;

    if a.dimensions() != b.dimensions() {
        println!(
            "DIMENSION MISMATCH: {:?} ({a_path}) vs {:?} ({b_path})",
            a.dimensions(),
            b.dimensions()
        );
        process::exit(2);
    }

    let (w, h) = a.dimensions();
    let total = w as u64 * h as u64;
    let mut diff_pixels: u64 = 0;
    let mut sum_abs: u64 = 0;
    let mut sum_sq: u64 = 0;
    let mut max_diff: u8 = 0;
    let mut heatmap = out_path.as_ref().map(|_| image::RgbaImage::new(w, h));

    for y in 0..h {
        for x in 0..w {
            let pa = a.get_pixel(x, y).0;
            let pb = b.get_pixel(x, y).0;
            let mut pix_max = 0u8;
            for c in 0..3 {
                let d = (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u8;
                sum_abs += d as u64;
                sum_sq += d as u64 * d as u64;
                pix_max = pix_max.max(d);
            }
            max_diff = max_diff.max(pix_max);
            if pix_max > threshold {
                diff_pixels += 1;
            }
            if let Some(hm) = heatmap.as_mut() {
                // Dim grayscale of A, with differing pixels glowing red.
                let base = ((pa[0] as u16 + pa[1] as u16 + pa[2] as u16) / 3) as u8;
                let dim = base / 3 + 30;
                let r = (pix_max as u16 * 4).min(255) as u8;
                let other = if pix_max > threshold { dim / 2 } else { dim };
                hm.put_pixel(
                    x,
                    y,
                    image::Rgba([dim.saturating_add(r), other, other, 255]),
                );
            }
        }
    }

    let channels = (total * 3) as f64;
    let mae = sum_abs as f64 / channels;
    let rmse = (sum_sq as f64 / channels).sqrt();
    let pct = diff_pixels as f64 / total as f64 * 100.0;

    println!("Compare: {a_path}  vs  {b_path}");
    println!("  Size: {w}x{h} ({total} px)");
    println!("  Differing pixels (>{threshold}/channel): {diff_pixels} ({pct:.3}%)");
    println!("  MAE: {mae:.3}/255    RMSE: {rmse:.3}/255    Max channel diff: {max_diff}/255");

    if let (Some(hm), Some(op)) = (heatmap, out_path) {
        hm.save(&op)
            .map_err(|e| zpdf::Error::StreamDecode(format!("save {op}: {e}")))?;
        println!("  Diff heatmap: {op}");
    }

    Ok(())
}

fn cmd_text(args: &[String]) -> zpdf::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: zpdf text <file.pdf> [-p <page>] [--all]");
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut all = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                page_num = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "--all" => all = true,
            _ => {}
        }
        i += 1;
    }

    let data = fs::read(pdf_path).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;

    let page_indices: Vec<usize> = if all {
        (0..doc.page_count()).collect()
    } else {
        vec![page_num.saturating_sub(1)]
    };

    // ICC transforms are per-document; share the cache across pages.
    let mut icc_cache = zpdf::IccCache::new();

    for &pi in &page_indices {
        let page = doc.page(pi)?;
        let mut font_cache = doc.load_page_fonts(&page);
        let content_bytes = doc.page_content_bytes(&page)?;
        let mut image_cache = zpdf::ImageCache::new();

        let mut spans: Vec<zpdf::TextSpan> = Vec::new();
        {
            let interpreter = zpdf::ContentInterpreter::new(page.effective_box())
                .with_fonts(&mut font_cache)
                .with_document(doc.file(), &page.resources)
                .with_images(&mut image_cache)
                .with_colors(&mut icc_cache)
                .with_text_sink(&mut spans);
            let _ = interpreter.interpret(&content_bytes);
        }

        if all {
            println!("===== Page {} =====", pi + 1);
        }
        let text = zpdf::spans_to_text(spans, 2.0);
        println!("{text}");
    }

    Ok(())
}

fn cmd_render(args: &[String]) -> zpdf::Result<()> {
    if args.is_empty() {
        eprintln!(
            "Usage: zpdf render <file.pdf> [-p <page>] [-o <output.png>] [--dpi <dpi>] [--backend cpu|wgpu]"
        );
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut output = String::from("output.png");
    let mut dpi: f32 = 150.0;
    let mut backend = String::from("cpu");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                page_num = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "-o" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    output = s.clone();
                }
            }
            "--dpi" => {
                i += 1;
                dpi = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(150.0);
            }
            "--backend" => {
                i += 1;
                // The `_ => {}` arm below silently ignores unknown flags, so an
                // unparsed/typo'd backend value would otherwise render CPU silently.
                // Capture and validate explicitly at render time.
                backend = args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("--backend requires a value (cpu|wgpu)");
                    process::exit(2);
                });
            }
            _ => {}
        }
        i += 1;
    }

    let data = fs::read(pdf_path).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;

    let page_index = page_num.saturating_sub(1);
    let page = doc.page(page_index)?;

    // CropBox ∩ MediaBox: the rect actually rendered (falls back to MediaBox).
    let page_box = page.effective_box();

    println!(
        "Rendering page {} ({:.0}x{:.0} pt) at {} DPI...",
        page_num,
        page_box.width(),
        page_box.height(),
        dpi
    );

    // Load page fonts
    let mut font_cache = doc.load_page_fonts(&page);
    let initial_fonts = font_cache.len();
    let initial_with_data = (0..initial_fonts)
        .filter(|&i| {
            font_cache
                .get(i as u32)
                .map(|f| f.has_font_data())
                .unwrap_or(false)
        })
        .count();
    println!(
        "  Fonts loaded: {} ({} with embedded data)",
        initial_fonts, initial_with_data
    );

    // Decode content stream
    let content_bytes = doc.page_content_bytes(&page)?;
    println!("  Content stream: {} bytes decoded", content_bytes.len());

    // Interpret content stream → display list
    let mut image_cache = zpdf::ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let oc_config = doc.oc_config();
    let mut icc_cache = zpdf::IccCache::new();
    let mut interpreter = zpdf::ContentInterpreter::new(page_box)
        .with_page_rotation(page.rotate)
        .with_fonts(&mut font_cache)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut image_cache)
        .with_colors(&mut icc_cache)
        .with_annotations(&annotations);
    if let Some(oc) = &oc_config {
        interpreter = interpreter.with_optional_content(oc);
    }
    let display_list = interpreter.interpret(&content_bytes);

    if font_cache.len() > initial_fonts {
        println!(
            "  Form XObject fonts: {} additional",
            font_cache.len() - initial_fonts
        );
    }
    println!("  Display list: {} commands", display_list.commands.len());

    // Count command types
    let mut fills = 0;
    let mut strokes = 0;
    let mut glyphs = 0;
    let mut clips = 0;
    let mut images = 0;
    for cmd in &display_list.commands {
        match cmd {
            zpdf::display_list::RenderCommand::FillPath { .. } => fills += 1,
            zpdf::display_list::RenderCommand::StrokePath { .. } => strokes += 1,
            zpdf::display_list::RenderCommand::DrawGlyphRun(_) => glyphs += 1,
            zpdf::display_list::RenderCommand::PushClip { .. } => clips += 1,
            zpdf::display_list::RenderCommand::DrawImage(_) => images += 1,
            _ => {}
        }
    }
    println!("    Fills: {fills}, Strokes: {strokes}, Glyph runs: {glyphs}, Clips: {clips}, Images: {images}");

    // Render with the selected backend. The DisplayList above is backend-agnostic;
    // we branch only here. Both arms route through `save_rgba` for one encoder path.
    #[allow(unused_imports)]
    use zpdf::RenderBackend;
    let scale = dpi / 72.0;

    match backend.as_str() {
        #[cfg(feature = "cpu")]
        "cpu" => {
            let mut renderer = zpdf::cpu::CpuRenderer::new()
                .with_fonts(&font_cache)
                .with_images(&image_cache);
            let rendered: zpdf::cpu::RenderedPage = renderer
                .render_display_list(&display_list, scale)
                .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;
            println!(
                "  Rendered (cpu): {}x{} pixels",
                rendered.width, rendered.height
            );
            save_rgba(&output, rendered.width, rendered.height, &rendered.data)?;
        }
        #[cfg(not(feature = "cpu"))]
        "cpu" => {
            eprintln!("--backend cpu requires building with --features cpu");
            process::exit(1);
        }
        #[cfg(feature = "gpu")]
        "wgpu" => {
            let mut renderer = zpdf::gpu::WgpuRenderer::new()
                .with_fonts(&font_cache)
                .with_images(&image_cache);
            let rendered = renderer
                .render_display_list(&display_list, scale)
                .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;
            println!(
                "  Rendered (wgpu): {}x{} pixels",
                rendered.width, rendered.height
            );
            save_rgba(&output, rendered.width, rendered.height, &rendered.data)?;
        }
        #[cfg(not(feature = "gpu"))]
        "wgpu" => {
            eprintln!("--backend wgpu requires building with --features gpu");
            process::exit(1);
        }
        other => {
            eprintln!("unknown --backend '{other}' (expected cpu|wgpu)");
            process::exit(2);
        }
    }

    println!("  Saved to: {output}");

    Ok(())
}

/// Save a tight RGBA8 buffer (top-left origin, `len == w*h*4`) as a PNG. Shared by
/// both render backends so output goes through a single encoder path.
fn save_rgba(path: &str, w: u32, h: u32, data: &[u8]) -> zpdf::Result<()> {
    let img = image::RgbaImage::from_raw(w, h, data.to_vec())
        .ok_or_else(|| zpdf::Error::StreamDecode("rgba buffer size mismatch".into()))?;
    img.save(path)
        .map_err(|e| zpdf::Error::StreamDecode(format!("save {path}: {e}")))
}

fn cmd_debug_stream(args: &[String]) -> zpdf::Result<()> {
    if args.len() < 3 {
        eprintln!("Usage: zpdf debug-stream <file.pdf> <obj_num> <gen_num>");
        process::exit(1);
    }
    let data = fs::read(&args[0]).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;
    let obj_num: u32 = args[1].parse().unwrap_or(0);
    let gen_num: u16 = args[2].parse().unwrap_or(0);
    let id = zpdf::ObjectId(obj_num, gen_num);
    let decoded = doc.file().resolve_stream_data(id)?;
    let text = String::from_utf8_lossy(&decoded);
    println!("{text}");
    Ok(())
}
