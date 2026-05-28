use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: zpdf <command> [args...]");
        eprintln!("Commands: info, dump, render");
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "info" => cmd_info(&args[2..]),
        "dump" => cmd_dump(&args[2..]),
        "render" => cmd_render(&args[2..]),
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

    for i in 0..doc.page_count() {
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

fn cmd_render(args: &[String]) -> zpdf::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: zpdf render <file.pdf> [-p <page>] [-o <output.png>] [--dpi <dpi>]");
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut output = String::from("output.png");
    let mut dpi: f32 = 150.0;

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
            _ => {}
        }
        i += 1;
    }

    let data = fs::read(pdf_path).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;

    let page_index = page_num.saturating_sub(1);
    let page = doc.page(page_index)?;

    println!(
        "Rendering page {} ({:.0}x{:.0} pt) at {} DPI...",
        page_num,
        page.width(),
        page.height(),
        dpi
    );

    // Load page fonts
    let font_cache = doc.load_page_fonts(&page);
    let fonts_with_data = (0..font_cache.len())
        .filter(|&i| font_cache.get(i as u32).map(|f| f.has_font_data()).unwrap_or(false))
        .count();
    println!(
        "  Fonts loaded: {} ({} with embedded data)",
        font_cache.len(),
        fonts_with_data
    );

    // Decode content stream
    let content_bytes = doc.page_content_bytes(&page)?;
    println!("  Content stream: {} bytes decoded", content_bytes.len());

    // Interpret content stream → display list
    let mut image_cache = zpdf::ImageCache::new();
    let interpreter = zpdf::ContentInterpreter::new(page.media_box)
        .with_fonts(&font_cache)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut image_cache);
    let display_list = interpreter.interpret(&content_bytes);
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

    // Render with CPU backend
    use zpdf::RenderBackend;
    let scale = dpi / 72.0;
    let mut renderer = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&font_cache)
        .with_images(&image_cache);
    let rendered: zpdf::cpu::RenderedPage = renderer
        .render_display_list(&display_list, scale)
        .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;

    println!(
        "  Rendered: {}x{} pixels",
        rendered.width, rendered.height
    );

    // Save PNG
    rendered
        .save_png(&output)
        .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;

    println!("  Saved to: {output}");

    Ok(())
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
