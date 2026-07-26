# zpdf-pptx-export Examples

## Command Line Usage

### Basic Export

Export all pages of a PDF to PowerPoint:

```bash
zpdf export-pptx input.pdf -o output.pptx --all
```

### Select Specific Pages

Export only certain pages:

```bash
# Single page
zpdf export-pptx report.pdf -o slide.pptx --pages 1

# Multiple pages
zpdf export-pptx report.pdf -o presentation.pptx --pages 1,3,5

# Page range
zpdf export-pptx report.pdf -o slides.pptx --pages 1-5

# Mixed
zpdf export-pptx report.pdf -o slides.pptx --pages 1,3-5,7,10-12
```

### Password-Protected PDFs

```bash
zpdf export-pptx secure.pdf -o output.pptx --password secret123 --all
```

## Library API Usage

### Basic Conversion

```rust
use zpdf::{Document, ContentInterpreter};
use zpdf_pptx_export::{display_list_to_slide, PptxPresentation, ExportOptions};

// Open the PDF
let data = std::fs::read("input.pdf")?;
let doc = Document::open(Arc::new(data))?;

let mut pptx_slides = Vec::new();

// Convert each page
for page_index in 0..doc.page_count() {
    let page = doc.page(page_index)?;
    let page_box = page.effective_box();
    
    // Interpret the content stream
    let mut interpreter = ContentInterpreter::new(page_box);
    interpreter.interpret_page(&page)?;
    let display_list = interpreter.into_display_list();
    
    // Get font and image caches
    let font_cache = page.font_cache();
    let image_cache = page.image_cache();
    
    // Convert to PowerPoint slide
    let options = ExportOptions::default();
    let slide = display_list_to_slide(
        &display_list,
        page_index,
        &font_cache,
        &image_cache,
        &options,
    )?;
    
    pptx_slides.push(slide);
}

// Create presentation
let presentation = PptxPresentation {
    slides: pptx_slides,
    width_emu: 9144000,   // 10 inches
    height_emu: 6858000,  // 7.5 inches
};

// Save to file
presentation.write_to_file("output.pptx")?;
```

### Custom Slide Dimensions

```rust
use zpdf_pptx_export::PptxPresentation;

// Create presentation with custom dimensions
// 1 inch = 914400 EMUs (English Metric Units)

// 16:9 widescreen (10" x 5.625")
let presentation = PptxPresentation {
    slides: vec![],
    width_emu: 9144000,
    height_emu: 5143500,
};

// A4 landscape (297mm x 210mm)
let presentation = PptxPresentation {
    slides: vec![],
    width_emu: 10_751_400,  // 297mm
    height_emu: 7_620_000,  // 210mm
};

// Custom size (12" x 9")
let presentation = PptxPresentation {
    slides: vec![],
    width_emu: 10_972_800,  // 12 inches
    height_emu: 8_229_600,  // 9 inches
};
```

### With Export Options

```rust
use zpdf_pptx_export::{ExportOptions, display_list_to_slide};

let options = ExportOptions {
    // Add future configuration options here
    // For example: text_grouping_threshold, shape_simplification, etc.
};

let slide = display_list_to_slide(
    &display_list,
    page_index,
    &font_cache,
    &image_cache,
    &options,
)?;
```

## Output Structure

The exported PowerPoint file contains:

### Text Elements
- **Font preservation**: Family name, size, bold, italic
- **Color**: RGB fill color from PDF
- **Position**: Accurate placement matching PDF layout
- **Content**: Unicode text extracted via ToUnicode CMap

### Shapes
- **Rectangles**: Axis-aligned rectangles with fill and stroke
- **Ellipses**: Circles and ellipses detected from Bézier curves
- **Lines**: Simple line segments
- **Custom paths**: Complex vector paths using Bézier curves

### Images
- **Format**: Embedded as PNG images
- **Transform**: Position, scale, and rotation preserved
- **Alpha**: Transparency maintained

## Limitations & Workarounds

### Complex Clipping

**Issue**: PowerPoint has limited clipping support compared to PDF.

**Workaround**: Complex clipping regions are flattened to images during export.

### Transparency Groups

**Issue**: PDF blend modes and soft masks have no PowerPoint equivalent.

**Workaround**: Transparency groups are rasterized to RGBA PNG images.

### Pattern Fills

**Issue**: PowerPoint supports limited pattern fills compared to PDF.

**Workaround**: Patterns fall back to simplified representations or solid colors.

### Text Without ToUnicode

**Issue**: Some PDFs don't include ToUnicode CMaps for text extraction.

**Workaround**: Text may appear as placeholder boxes. Consider using OCR or source files.

### Rotated Text

**Issue**: Current implementation doesn't handle text rotation.

**Workaround**: Future enhancement will detect transform matrices and apply PowerPoint rotation.

## Tips for Best Results

1. **Source PDFs**: PDFs generated from Office applications convert best
2. **Fonts**: PDFs with embedded fonts produce more accurate results
3. **Vector Graphics**: Simple shapes convert better than complex artwork
4. **Text Layout**: Left-aligned text converts more accurately than complex layouts
5. **File Size**: PDFs with many images will produce larger PPTX files

## Troubleshooting

### Empty Slides

If slides appear empty:
- Check if the PDF uses advanced features (form XObjects, transparency)
- Verify text has ToUnicode mapping: `zpdf text input.pdf -p 1`
- Try exporting a single page first to isolate issues

### Misplaced Elements

If elements are incorrectly positioned:
- Verify PDF page boxes: `zpdf info input.pdf`
- Check for non-standard coordinate transforms
- Report edge cases for improvement

### Missing Text

If text doesn't appear:
- Check font embedding: `zpdf info input.pdf`
- Verify ToUnicode CMap exists in the PDF
- Try text extraction: `zpdf text input.pdf -p 1`
