# zpdf-pptx-export

PowerPoint (PPTX) export functionality for zpdf - converts PDF pages to editable PowerPoint presentations.

## Overview

This crate provides PDF-to-PowerPoint conversion that preserves content editability rather than simply converting pages to images. It extracts structured content from PDF pages (text, shapes, images) and maps them to OOXML PowerPoint format.

## Features

- **Editable Text**: Text is extracted and preserved as PowerPoint text boxes with formatting (font, size, color, bold, italic)
- **Shape Recognition**: Recognizes and converts rectangles, ellipses, and lines as native PowerPoint shapes
- **Image Embedding**: Images are extracted and embedded as PNG files
- **Vector Graphics**: Complex paths are converted to custom PowerPoint geometry
- **Coordinate Mapping**: Properly handles PDF's bottom-left origin to PowerPoint's top-left origin

## Architecture

The conversion pipeline follows zpdf's design principles:

```
PDF bytes
  → zpdf-parser      (parse PDF structure)
  → zpdf-document    (page tree, fonts)
  → zpdf-content     (content stream interpretation)
  → zpdf-display-list (flat RenderCommand sequence)
  → zpdf-pptx-export (convert to OOXML PowerPoint)
```

### Key Components

- **lib.rs**: Main conversion logic, processes display list commands
- **ooxml.rs**: OOXML PowerPoint XML generation and ZIP packaging
- **shape_recognizer.rs**: Recognizes geometric shapes (rectangles, ellipses, lines)
- **text_grouper.rs**: Groups text spans into coherent text boxes

## Usage

### CLI

```bash
# Export all pages
cargo run -p zpdf-cli -- export-pptx input.pdf -o output.pptx --all

# Export specific pages
cargo run -p zpdf-cli -- export-pptx input.pdf -o output.pptx --pages 1,3-5

# Export with password-protected PDF
cargo run -p zpdf-cli -- export-pptx input.pdf -o output.pptx --password secret
```

### Library API

```rust
use zpdf_pptx_export::{display_list_to_slide, PptxPresentation, ExportOptions};

// Convert a display list to a PowerPoint slide
let slide = display_list_to_slide(
    &display_list,
    page_index,
    &font_cache,
    &image_cache,
    &options,
)?;

// Create a presentation with multiple slides
let presentation = PptxPresentation {
    slides: vec![slide],
    width_emu: 9144000,  // 10 inches in EMUs
    height_emu: 6858000, // 7.5 inches in EMUs
};

// Write to file
presentation.write_to_file("output.pptx")?;
```

## Element Mapping

| PDF Element | PowerPoint Shape | Notes |
|-------------|-----------------|-------|
| TextSpan | `<p:sp>` with `<p:txBody>` | Font, size, color preserved |
| Rectangle | `<p:sp>` with `prst="rect"` | Fill and stroke converted |
| Ellipse | `<p:sp>` with `prst="ellipse"` | Detected from 4-curve paths |
| Line | `<p:cxnSp>` | Connector shape |
| Complex Path | `<p:sp>` with `<a:custGeom>` | Custom Bézier path |
| Image | `<p:pic>` | Embedded as PNG |

## Limitations

- **Text Extraction**: Relies on ToUnicode CMap; some PDFs may have incomplete text mapping
- **Clipping**: Complex clipping paths are flattened to images
- **Transparency Groups**: Soft masks and blend modes are rasterized
- **Advanced Features**: Patterns, shadings, and some paint types fall back to simplified representations

## Coordinate Systems

- **PDF**: Origin at bottom-left, Y+ upward, units in points (1/72 inch)
- **PowerPoint**: Origin at top-left, Y+ downward, units in EMUs (914,400 EMUs = 1 inch)

The converter handles coordinate transformation automatically.

## Design Decisions

Following zpdf's architecture:

1. **Pure Rust**: No C/C++ dependencies, uses `zip` crate for PPTX packaging
2. **Display List Input**: Operates on the flat `RenderCommand` sequence, not raw PDF objects
3. **Lazy Processing**: Only processes requested pages
4. **Error Handling**: Uses `thiserror` for consistent error types

## Future Enhancements

- Better text grouping (paragraph detection, column layout)
- Table detection and conversion to PowerPoint tables
- Animation and transition support (from PDF page transitions)
- Master slide templates
- Speaker notes from PDF annotations
