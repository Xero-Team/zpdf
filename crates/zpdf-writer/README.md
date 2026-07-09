# zpdf-writer — PDF Editing via Incremental Updates

New crate providing PDF editing capabilities through incremental updates (append-only modifications that preserve the original file).

## Features

### 1. Core Infrastructure (`lib.rs`)
- **IncrementalWriter**: Main API for PDF editing
  - Pending updates map (`ObjectId → PdfObject`)
  - Automatic xref stream support (writes `/Type /XRef` when original uses it)
  - `/Info` and `/ID` inheritance fix (reads back through `/Prev` chain)
  - `write<W: Write + Seek>()` serializes to any seekable writer

### 2. Metadata Editing (`metadata.rs`)
- **InfoUpdate**: Flexible metadata updates
  - `Option<Option<String>>` pattern: `None` = no change, `Some(None)` = delete, `Some(Some(s))` = set
  - Supports: title, author, subject, keywords, creator, producer
  - Auto-adds `/ModDate` in UTC
- **PDF text string encoding**: UTF-16BE for non-ASCII, literal for ASCII

### 3. Form Filling (`forms.rs`)
- **FormFiller**: AcroForm field manipulation
  - Parses field tree (recursive `/Kids`, handles nesting)
  - `set(name, value)` updates field value + appearance
  - Text fields: sets `/V` + regenerates `/AP /N` appearance stream
  - Checkboxes: sets `/V` + `/AS` to `/Yes` or `/Off`, renders checkmark/X
  - Read-only fields warned and skipped

### 4. Page Operations (`pages.rs`)
- **rotate_page(index, degrees)**: Cumulative rotation (0/90/180/270)
- **delete_pages(indices)**: Batch deletion, updates `/Kids` and `/Count`
- **reorder_pages(new_order)**: Arbitrary page reordering

### 5. Content Stamping (`stamp.rs`)
- **StampItem**: Text or image overlays
  - Text: font/size/color/position (Standard 14 fonts)
  - Image: JPEG (DCTDecode passthrough), RGB8 (FlateDecode), RGBA8 (with `/SMask`)
- **stamp_page**: Wraps all items in Form XObject, appends via q/Q sandwich
  - `[q <original> Q q /ZPDFStampN Do Q]` isolates stamp in separate graphics state
  - Merges `/Resources`, auto-renames on collision
- **jpeg_dimensions**: SOF0/1/2 parser for dimension extraction

### 6. CLI Commands (`zpdf-cli`)
- `zpdf fill <in.pdf> --set NAME=VALUE -o <out.pdf>` (+ `--list` to enumerate fields)
- `zpdf pages <in.pdf> --rotate PAGES:DEG --delete LIST --order LIST -o <out.pdf>`
- `zpdf set-meta <in.pdf> --title S --author S ... -o <out.pdf>`
- `zpdf stamp <in.pdf> -p N --text STR --at X,Y --font F --size S --color R,G,B -o <out.pdf>`
- All support `--password <pw>`, enforce input ≠ output

## Architecture

```
zpdf-writer
  ├─ lib.rs           IncrementalWriter, xref serialization, trailer inheritance
  ├─ metadata.rs      InfoUpdate, PDF text string encoding
  ├─ forms.rs         FormFiller, field tree parsing, appearance generation
  ├─ pages.rs         rotate/delete/reorder operations
  ├─ stamp.rs         StampItem, Form XObject wrapping, JPEG parsing
  └─ serialize.rs     Low-level PDF object/xref/trailer serialization

zpdf (facade)
  └─ Re-exports: IncrementalWriter, FormFiller, InfoUpdate, StampItem, StampImage

zpdf-cli
  └─ Commands: fill, pages, set-meta, stamp (+ shared utilities)
```

## Dependencies
- **zpdf-document**: Reuses `generate_widget_appearance`, `standard_font_dict`, `escape_text`
- **zpdf-parser/core**: Object model, `PdfFile` for xref/trailer reading
- **flate2**: FlateDecode compression (existing dependency)
- **Pure Rust, zero new C dependencies**

## Testing
- **Unit tests**: UTF-16BE encoding, JPEG SOF parsing
- **Manual end-to-end**: Verified metadata, rotation, stamping via `zpdf info` and rendering
- **Robustness**: ParseLimits enforced, field tree cycle detection, JPEG marker validation

## Example Usage

```rust
use zpdf::{IncrementalWriter, InfoUpdate, StampItem};
use std::fs::File;

// Edit metadata
let mut writer = IncrementalWriter::new(pdf_bytes)?;
writer.set_info(&InfoUpdate {
    title: Some(Some("My Document".into())),
    author: Some(Some("John Doe".into())),
    ..Default::default()
})?;
writer.write(&mut File::create("out.pdf")?)?;

// Stamp text
let mut writer = IncrementalWriter::new(pdf_bytes)?;
writer.stamp_page(0, &[StampItem::Text {
    text: "CONFIDENTIAL".into(),
    x: 200.0, y: 700.0,
    font: "Helvetica-Bold".into(),
    size: 36.0,
    color: (1.0, 0.0, 0.0),
}])?;
writer.write(&mut File::create("stamped.pdf")?)?;
```

## CLI Examples

```bash
# Set metadata
zpdf set-meta input.pdf --title "Report" --author "Alice" -o output.pdf

# Fill form
zpdf fill form.pdf --set "Name=John Doe" --set "Agree=true" -o filled.pdf

# Rotate and delete pages
zpdf pages doc.pdf --rotate 0,2-5:90 --delete 10 -o edited.pdf

# Stamp watermark
zpdf stamp doc.pdf -p 1 --text "DRAFT" --at 300,400 --size 48 --color 1,0,0 -o draft.pdf
```

## Implementation Notes

1. **Incremental updates preserve digital signatures** (though edits may invalidate them — CLI warns)
2. **q/Q sandwich** for stamps ensures unbalanced original streams don't corrupt output
3. **Inherited `/Resources` handling**: Dereferences and merges rather than replacing
4. **xref stream auto-detection**: Matches original format (table vs stream)
5. **Form appearance regeneration**: Uses Standard 14 fonts (no embedding needed)
6. **Page tree recursion**: Handles nested `/Pages` nodes correctly

## Future Enhancements (not yet implemented)
- Image stamp support (currently only text)
- Multi-page stamping (currently one page at a time)
- Form field creation (currently only fills existing fields)
- Annotation editing (currently read-only)
- Encryption/decryption on write
