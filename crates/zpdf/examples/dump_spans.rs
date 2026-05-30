//! Debug helper: dump extracted text spans (x, y, size, advance, text) for a page.
//! Usage: cargo run -p zpdf --example dump_spans -- <file.pdf> <page>

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, TextSpan};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let page_num: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);

    let data = std::fs::read(path).unwrap();
    let doc = PdfDocument::open(data).unwrap();
    let page = doc.page(page_num - 1).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).unwrap();
    let mut images = ImageCache::new();
    let mut spans: Vec<TextSpan> = Vec::new();
    {
        let interp = ContentInterpreter::new(page.media_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let _ = interp.interpret(&content);
    }
    eprintln!("# {} spans, mediabox {:?}", spans.len(), page.media_box);
    for s in &spans {
        println!(
            "x={:7.1} y={:7.1} sz={:5.1} adv={:7.1} {:?}",
            s.x, s.y, s.size, s.advance, s.text
        );
    }
}
