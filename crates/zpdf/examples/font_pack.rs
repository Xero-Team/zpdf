//! Render with a caller-selected cached font, without installed fonts or I/O in
//! the renderer. Run tools/fetch_font.py first; see docs/guides/font-provider.md.

use std::io::{Read, Write};
use std::sync::Arc;
use zpdf::{FontPack, FontPackFormat, FontPackLimits, FontProvider, PdfDocument, RenderBackend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 {
        return Err(
            "usage: font_pack INPUT.pdf CACHE.font.zlib PDF_FONT_ALIAS DECODED_LIMIT OUTPUT.ppm"
                .into(),
        );
    }
    let limits = FontPackLimits {
        max_decoded_bytes: args[4].parse()?,
        ..Default::default()
    };
    let mut encoded = Vec::new();
    std::fs::File::open(&args[2])?
        .take(limits.max_encoded_bytes + 1)
        .read_to_end(&mut encoded)?;
    let provider = Arc::new(FontProvider::memory_only());
    provider.register_pack(
        Arc::new(FontPack::new(encoded, FontPackFormat::Zlib, limits)?),
        0,
        &[&args[3]],
    )?;
    let doc = PdfDocument::open(std::fs::read(&args[1])?)?.with_font_provider(provider);
    let page = doc.page(0)?;
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = zpdf::ImageCache::new();
    let mut colors = zpdf::IccCache::new();
    let dl = zpdf::ContentInterpreter::new(page.effective_box())
        .with_page_rotation(page.rotate)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_colors(&mut colors)
        .interpret(&doc.page_content_bytes(&page)?);
    let bitmap = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .with_limits(doc.file().limits())
        .render_display_list(&dl, 2.0)?;
    let mut output = std::io::BufWriter::new(std::fs::File::create(&args[5])?);
    write!(output, "P6\n{} {}\n255\n", bitmap.width, bitmap.height)?;
    for pixel in bitmap.data.as_chunks::<4>().0 {
        output.write_all(&pixel[..3])?;
    }
    output.flush()?;
    Ok(())
}
