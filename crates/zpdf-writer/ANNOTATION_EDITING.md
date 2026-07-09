# Annotation Editing API

This document describes the new annotation editing capabilities added to `zpdf-writer` in Phase 1 of the interactive PDF editing implementation.

## Overview

The `IncrementalWriter` now provides methods to modify existing PDF annotations without rewriting the entire document. These operations use PDF's incremental update mechanism (ISO 32000-1 §7.5.6), preserving the original file structure.

## New Methods

### 1. Delete Annotation

```rust
pub fn delete_annotation(&mut self, page_index: usize, annot_id: ObjectId) -> Result<()>
```

Removes an annotation from a page's `/Annots` array. The annotation object remains in the file but is no longer rendered.

**Example:**
```rust
let pdf_bytes = std::fs::read("input.pdf")?;
let mut writer = IncrementalWriter::new(pdf_bytes)?;
writer.delete_annotation(0, ObjectId(5, 0))?;
writer.write(&mut std::fs::File::create("output.pdf")?)?;
```

### 2. Update Annotation Rectangle

```rust
pub fn update_annotation_rect(&mut self, annot_id: ObjectId, new_rect: Rect) -> Result<()>
```

Moves or resizes an annotation by updating its `/Rect` entry. For ink annotations and stamps with relative-coordinate appearance streams, this is sufficient to reposition them.

**Example:**
```rust
let new_rect = Rect { x0: 100.0, y0: 200.0, x1: 300.0, y1: 400.0 };
writer.update_annotation_rect(ObjectId(5, 0), new_rect)?;
```

**Note:** This only updates the bounding rectangle. For annotations with absolute-coordinate appearance streams, the visual appearance may not follow the new position. Future enhancements may add appearance stream regeneration.

### 3. Update Annotation Color

```rust
pub fn update_annotation_color(&mut self, annot_id: ObjectId, color: (f64, f64, f64)) -> Result<()>
```

Updates an annotation's stroke/border color via the `/C` entry (RGB values in 0.0–1.0 range).

**Example:**
```rust
writer.update_annotation_color(ObjectId(5, 0), (1.0, 0.0, 0.0))?; // Red
```

**Note:** Updates the annotation dictionary but does not regenerate the appearance stream. The color change may not be visible in all viewers unless the appearance is also regenerated.

### 4. Update Annotation Contents

```rust
pub fn update_annotation_contents(&mut self, annot_id: ObjectId, content: &str) -> Result<()>
```

Updates a text annotation's content string (the `/Contents` entry). Automatically handles UTF-16BE encoding for non-ASCII text.

**Example:**
```rust
writer.update_annotation_contents(ObjectId(5, 0), "Updated comment")?;
```

### 5. Update Annotation Border Width

```rust
pub fn update_annotation_border_width(&mut self, annot_id: ObjectId, width: f64) -> Result<()>
```

Updates an annotation's border width via the `/BS /W` entry. Creates the `/BS` dictionary if it doesn't exist. Enforces a minimum width of 0.1 points.

**Example:**
```rust
writer.update_annotation_border_width(ObjectId(5, 0), 3.0)?;
```

## Usage Pattern

All annotation editing methods follow the same pattern:

1. **Create writer** from original PDF bytes
2. **Apply edits** via method calls (can chain multiple edits)
3. **Write output** to a seekable writer (file or `Cursor`)

```rust
use zpdf_writer::IncrementalWriter;
use zpdf_core::{ObjectId, Rect};
use std::fs::File;

// Load PDF
let pdf_bytes = std::fs::read("input.pdf")?;
let mut writer = IncrementalWriter::new(pdf_bytes)?;

// Apply multiple edits
writer.delete_annotation(0, ObjectId(10, 0))?;
writer.update_annotation_rect(ObjectId(5, 0), Rect { x0: 100.0, y0: 200.0, x1: 300.0, y1: 400.0 })?;
writer.update_annotation_color(ObjectId(5, 0), (0.0, 1.0, 0.0))?;

// Save
writer.write(&mut File::create("output.pdf")?)?;
```

## Edit Composition

Edits compose correctly when targeting the same annotation:

```rust
// Move and recolor the same annotation
writer.update_annotation_rect(ObjectId(5, 0), new_rect)?;
writer.update_annotation_color(ObjectId(5, 0), (1.0, 0.0, 0.0))?;
// Both changes are applied in a single incremental update
```

The `resolve_current()` method ensures later edits see pending changes from earlier edits.

## Limitations

### 1. Appearance Stream Regeneration
When moving or resizing annotations, the `/AP /N` appearance stream is not automatically regenerated. For annotations where the appearance uses absolute coordinates (some stamps, form widgets), the visual appearance may not match the new `/Rect`.

**Works correctly:**
- Ink annotations (appearance uses relative coordinates within BBox)
- Simple geometric annotations with relative appearance

**May need appearance regeneration (future enhancement):**
- Text annotations with positioned text
- Stamps with absolute coordinates
- Form widgets

### 2. Encrypted Documents
Encrypted PDFs cannot be incrementally updated (the writer rejects them at construction time). Decryption would be required first.

### 3. Form Field Widgets
Widget annotations tied to AcroForm fields can be moved, but the field value and appearance may need separate handling. Full form field editing will be added in Phase 3.

## Testing

Unit tests verify that:
- Annotations can be deleted (disappear from `/Annots` array)
- Rectangle, color, and border width updates execute without error
- Output PDFs are valid and parseable

Run tests with:
```bash
cargo test -p zpdf-writer
```

## Integration with zpdf-viewer-gpui

Phase 2 will add a visual selection tool to the GPUI viewer demonstrating these APIs:
- Click to select annotations
- Drag to move (calls `update_annotation_rect`)
- Delete key (calls `delete_annotation`)
- Properties panel (calls property update methods)
- Buffered save with Ctrl+S

## Future Enhancements (Phase 4)

- Appearance stream regeneration for moved/resized annotations
- Content stream editing (modify page text/images, not just annotations)
- Support for other annotation types (FreeText, Highlight, etc.)
- Batch operations (delete multiple annotations in one call)
