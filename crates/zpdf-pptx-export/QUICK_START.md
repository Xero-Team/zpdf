# Quick Start: PDF to PowerPoint Export

## Command Line

```bash
# Export all pages
zpdf export-pptx input.pdf -o output.pptx --all

# Export specific pages
zpdf export-pptx input.pdf -o output.pptx --pages 1,3-5,7

# With password
zpdf export-pptx secure.pdf -o output.pptx --password secret123 --all
```

## Rust API

```rust
use std::sync::Arc;
use zpdf::{Document, ContentInterpreter};
use zpdf_pptx_export::{display_list_to_slide, PptxPresentation, ExportOptions};

fn convert_pdf_to_pptx(pdf_path: &str, pptx_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Open PDF
    let data = std::fs::read(pdf_path)?;
    let doc = Document::open(Arc::new(data))?;
    
    let mut slides = Vec::new();
    
    // Convert each page
    for page_index in 0..doc.page_count() {
        let page = doc.page(page_index)?;
        let page_box = page.effective_box();
        
        // Interpret content
        let mut interpreter = ContentInterpreter::new(page_box);
        interpreter.interpret_page(&page)?;
        let display_list = interpreter.into_display_list();
        
        // Convert to slide
        let slide = display_list_to_slide(
            &display_list,
            page_index,
            &page.font_cache(),
            &page.image_cache(),
            &ExportOptions::default(),
        )?;
        
        slides.push(slide);
    }
    
    // Save presentation
    let presentation = PptxPresentation {
        slides,
        width_emu: 9144000,   // 10 inches
        height_emu: 6858000,  // 7.5 inches
    };
    
    presentation.write_to_file(pptx_path)?;
    Ok(())
}
```

## What Gets Converted

✅ **Text** — Font family, size, color, bold, italic  
✅ **Shapes** — Rectangles, ellipses, lines, custom paths  
✅ **Images** — Embedded as PNG with position/scale  
✅ **Colors** — RGB fill and stroke colors  

⚠️ **Limitations**:
- Complex clipping → rasterized
- Transparency groups → rasterized
- Pattern fills → simplified
- Text rotation → not yet supported

## Tips

1. **Best source PDFs**: Office-generated PDFs with embedded fonts
2. **Text quality**: PDFs need ToUnicode CMaps for text extraction
3. **File size**: Image-heavy PDFs produce larger PPTX files
4. **Verification**: `zpdf text input.pdf -p 1` to check text extraction

## Troubleshooting

**Empty slides?**
```bash
# Check if text is extractable
zpdf text input.pdf -p 1

# Try single page first
zpdf export-pptx input.pdf -o test.pptx --pages 1
```

**Missing text?**
```bash
# Check font embedding
zpdf info input.pdf | grep -i font
```

**Wrong positions?**
```bash
# Check page boxes
zpdf info input.pdf | grep -i box
```

## More Info

- Full documentation: `EXAMPLES.md`
- Implementation details: `../../docs/review/PDF_TO_PPTX_IMPLEMENTATION.md`
- API reference: https://docs.rs/zpdf-pptx-export
