# Selection Mode - Interactive Annotation Editing

This document describes the selection mode GUI implementation in `zpdf-viewer-gpui`, demonstrating the annotation editing APIs from `zpdf-writer`.

## Overview

Selection mode provides an Acrobat-like interface for selecting, moving, and deleting PDF annotations. It serves as both a useful feature and a reference implementation for developers building their own PDF editors.

## Features

### 1. Selection Tool (Press `s`)

Toggle selection mode with the `s` key or the "🖱 Select" button in the toolbar.

When active:
- Click annotations to select them
- Selected annotations show a blue border
- Annotation count displayed in toolbar
- Pending edits counter shows unsaved changes

### 2. Move Annotations

**Drag to move:**
1. Enter selection mode (`s`)
2. Click an annotation to select it
3. Drag to move it to a new position
4. Blue border follows during drag
5. Release to record the move in the edit buffer

The move is **buffered** - not immediately saved to disk. Press `Ctrl+S` to save.

### 3. Delete Annotations (Press `Delete`)

**Delete selected annotation:**
1. Enter selection mode (`s`)
2. Click an annotation to select it
3. Press `Delete` or `Backspace`
4. Annotation disappears immediately
5. Delete operation recorded in edit buffer

Press `Ctrl+S` to save the deletion permanently.

### 4. Save Edits (Press `Ctrl+S`)

**Flush edit buffer:**
1. Make changes (move or delete annotations)
2. Status bar shows "X edit(s) pending"
3. Press `Ctrl+S` to save all changes
4. Incremental update appended to PDF
5. Edit buffer cleared
6. Status shows "✓ Saved X edit(s)"

All changes are written as a single incremental update using `zpdf-writer::IncrementalWriter`.

## Architecture

### Data Structures

```rust
// Annotation tracking
struct EditableAnnotation {
    object_id: ObjectId,
    page_index: usize,
    subtype: String,
    original_rect: Rect,
    current_rect: Rect,  // Updated during drag
}

// Pending edit operations
enum EditOperation {
    Move { object_id, page_index, new_rect },
    Delete { object_id, page_index },
}

// Edit buffer (keyed by ObjectId for last-write-wins)
struct EditBuffer {
    operations: HashMap<ObjectId, EditOperation>,
}

// Drag state
enum DragState {
    MovingAnnotation {
        annotation_id: ObjectId,
        start_screen_pos: (f32, f32),
        start_pdf_rect: Rect,
    },
}
```

### Coordinate Systems

**Screen → PDF conversion:**
```rust
fn screen_to_pdf(screen_x, screen_y, page, zoom) -> (pdf_x, pdf_y) {
    let scale = (PREVIEW_DPI / 72.0) * zoom;
    let pdf_x = (screen_x / scale) as f64;
    let pdf_y = page.height - (screen_y / scale);  // Flip Y axis
    (pdf_x, pdf_y)
}
```

- **Screen**: origin top-left, Y+ down, pixels
- **PDF**: origin bottom-left, Y+ up, points (1/72 inch)
- **Scale**: `(144 DPI / 72.0) * zoom`

**PDF → Screen conversion (for rendering selection UI):**
```rust
let scale = (PREVIEW_DPI / 72.0) * zoom;
let screen_x0 = (pdf_rect.x0 * scale) as f32;
let screen_y0 = ((page.height - pdf_rect.y1) * scale) as f32;  // Flip Y
let screen_width = ((pdf_rect.x1 - pdf_rect.x0) * scale) as f32;
let screen_height = ((pdf_rect.y1 - pdf_rect.y0) * scale) as f32;
```

### State Machine

```
AnnotationMode::None
   ↓ (press 's')
AnnotationMode::Select
   ↓ load_page_annotations()
   ↓ (annotations loaded into page_annotations vec)
   ↓
   ├─ Click annotation → selected_annotation = Some(id)
   │                      Blue border rendered
   │
   ├─ Drag annotation → DragState::MovingAnnotation
   │                     Update current_rect in page_annotations
   │                     On mouse up → EditOperation::Move added to buffer
   │
   ├─ Press Delete → EditOperation::Delete added to buffer
   │                 Remove from page_annotations
   │                 selected_annotation = None
   │
   └─ Press Ctrl+S → save_edits() flushes buffer to PDF
                      Clears edit_buffer
                      Reloads annotations
```

### Rendering

**Selection overlay** (lines 1080-1095 in viewer.rs):
```rust
if self.annotation_mode == AnnotationMode::Select {
    if let Some(selected_id) = self.selected_annotation {
        // Find annotation and convert rect to screen coordinates
        let scale = (PREVIEW_DPI / 72.0) * zoom;
        let screen_x0 = ...;
        let screen_y0 = ...;
        
        // Render blue border
        container.child(
            div()
                .absolute()
                .left(px(screen_x0))
                .top(px(screen_y0))
                .w(px(screen_width))
                .h(px(screen_height))
                .border_2()
                .border_color(rgb(0x4A90E2))
        );
    }
}
```

### Mouse Event Handling

**Mouse down** (lines 670-690):
```rust
fn handle_select_mouse_down(event, window, cx) {
    let hit = hit_test(event.position.x, event.position.y, page, zoom);
    match hit {
        HitTestResult::AnnotationBody { annotation_id } => {
            self.selected_annotation = Some(annotation_id);
            self.drag_state = Some(DragState::MovingAnnotation { ... });
        }
        HitTestResult::None => {
            self.selected_annotation = None;
        }
    }
}
```

**Mouse move** (lines 698-735):
```rust
fn handle_select_mouse_move(event, window, cx) {
    if let Some(DragState::MovingAnnotation { start_pos, start_rect, ... }) = drag_state {
        // Calculate delta in screen space
        let screen_dx = event.position.x - start_pos.0;
        let screen_dy = event.position.y - start_pos.1;
        
        // Convert to PDF space (Y-flip!)
        let pdf_dx = (screen_dx / scale) as f64;
        let pdf_dy = -((screen_dy / scale) as f64);
        
        // Update annotation's current_rect
        annot.current_rect = Rect {
            x0: start_rect.x0 + pdf_dx,
            y0: start_rect.y0 + pdf_dy,
            x1: start_rect.x1 + pdf_dx,
            y1: start_rect.y1 + pdf_dy,
        };
    }
}
```

**Mouse up** (lines 737-756):
```rust
fn handle_select_mouse_up(event, cx) {
    if let Some(DragState::MovingAnnotation { annotation_id, ... }) = drag_state {
        // Find final rect
        let final_rect = annot.current_rect;
        
        // Add to edit buffer
        self.edit_buffer.add(EditOperation::Move {
            object_id: annotation_id,
            page_index: self.current_page,
            new_rect: final_rect,
        });
        
        self.drag_state = None;
    }
}
```

### Saving Edits

**save_edits()** (lines 773-862):
```rust
fn save_edits(cx) {
    // Read original PDF
    let original_bytes = fs::read(&self.document.path)?;
    
    // Create incremental writer
    let mut writer = IncrementalWriter::new(original_bytes)?;
    
    // Apply all buffered operations
    for op in self.edit_buffer.operations.values() {
        match op {
            EditOperation::Move { object_id, new_rect, .. } => {
                writer.update_annotation_rect(*object_id, *new_rect)?;
            }
            EditOperation::Delete { object_id, page_index } => {
                writer.delete_annotation(*page_index, *object_id)?;
            }
        }
    }
    
    // Write to temp file, then atomic rename
    let temp_path = path.with_extension("pdf.tmp");
    writer.write(&mut File::create(&temp_path)?)?;
    fs::rename(&temp_path, &path)?;
    
    // Clear buffer and reload
    self.edit_buffer.clear();
    self.page_cache.clear();
    self.load_page_annotations(cx);
}
```

## Keybindings

| Key | Action | Description |
|-----|--------|-------------|
| `s` | Toggle Select Mode | Enter/exit selection mode |
| `Ctrl+S` / `Cmd+S` | Save Edits | Flush edit buffer to PDF |
| `Delete` / `Backspace` | Delete Selected | Delete selected annotation |
| Click | Select | Select annotation under cursor |
| Drag | Move | Move selected annotation |

## Usage Example

```bash
# Build the viewer
cargo build -p zpdf-viewer-gpui --release

# Run on a PDF with annotations
./target/release/zpdf-viewer-gpui path/to/annotated.pdf

# In the viewer:
# 1. Press 's' to enter selection mode
# 2. Click an annotation to select it (blue border appears)
# 3. Drag to move it
# 4. Press Delete to remove it
# 5. Press Ctrl+S to save changes
# 6. PDF is incrementally updated
```

## Limitations & Future Enhancements

### Current Limitations

1. **Appearance Stream Not Regenerated**
   - Moving an annotation updates its `/Rect` but not its appearance stream
   - Works correctly for ink annotations (relative coordinates)
   - May not work for stamps/form fields with absolute coordinates
   - Future: regenerate appearance streams on move

2. **Single-Page Only**
   - Annotations loaded per-page
   - Moving between pages clears selection
   - Future: persist selection across page navigation

3. **No Resize Handles**
   - `ResizeHandle` enum defined but not yet implemented
   - Can move but not resize annotations
   - Future: corner/edge handles for resizing

4. **No Properties Panel**
   - Can move and delete, but not edit color/border/content in GUI
   - Future: properties sidebar with text fields for annotation attributes

5. **No Undo/Redo**
   - Edit buffer is write-only
   - Once saved, changes are permanent (unless you have the previous PDF)
   - Future: undo stack with reversal operations

### Planned Enhancements (Phase 3)

- **Resize handles** - corner and edge handles for resizing annotations
- **Properties panel** - edit color, border width, content text, etc.
- **Multi-select** - select and move multiple annotations at once
- **Copy/paste** - duplicate annotations within or across pages
- **Undo/redo** - maintain operation history with reversal
- **Snap to grid** - align annotations to pixel grid
- **Keyboard shortcuts** - arrow keys for nudging, Shift+drag for constrained movement

## Integration with zpdf-writer APIs

This GUI directly demonstrates the following `zpdf-writer` APIs:

| GUI Action | API Call |
|------------|----------|
| Drag annotation | `writer.update_annotation_rect(id, new_rect)` |
| Delete annotation | `writer.delete_annotation(page, id)` |
| Save edits | `writer.write(&mut file)` |

Future properties panel will demonstrate:
- `writer.update_annotation_color(id, rgb)`
- `writer.update_annotation_contents(id, text)`
- `writer.update_annotation_border_width(id, width)`

## Code Organization

```
crates/zpdf-viewer-gpui/src/
├── actions.rs           # Action definitions (ToggleSelectMode, SaveEdits, DeleteSelected)
├── app.rs               # Keybindings registration
├── viewer.rs            # Main implementation
│   ├── Lines 30-106     # Selection data structures
│   ├── Lines 566-632    # load_page_annotations()
│   ├── Lines 634-648    # hit_test()
│   ├── Lines 650-662    # screen_to_pdf()
│   ├── Lines 664-690    # handle_select_mouse_down()
│   ├── Lines 692-735    # handle_select_mouse_move()
│   ├── Lines 737-756    # handle_select_mouse_up()
│   ├── Lines 758-771    # delete_selected_annotation()
│   ├── Lines 773-862    # save_edits()
│   ├── Lines 1080-1095  # Selection overlay rendering
│   ├── Lines 1142-1162  # Mouse event handler attachment
│   └── Lines 1590-1679  # Toolbar buttons and status display
└── document.rs          # Added document() accessor (line 119)
```

## Testing

Manual testing workflow:

1. **Create test PDF with annotations:**
   ```bash
   # Use the ink mode to add annotations
   cargo run -p zpdf-viewer-gpui test.pdf
   # Press 'i', draw some strokes, press 'Save'
   ```

2. **Test selection:**
   ```bash
   # Reopen the PDF
   cargo run -p zpdf-viewer-gpui test.pdf
   # Press 's' to enter selection mode
   # Click an annotation → should see blue border
   ```

3. **Test move:**
   ```bash
   # With annotation selected, drag to new position
   # Should see border follow cursor
   # Release → status shows "Moved annotation (press Ctrl+S to save)"
   ```

4. **Test delete:**
   ```bash
   # With annotation selected, press Delete
   # Annotation disappears
   # Status shows "Deleted annotation (press Ctrl+S to save)"
   ```

5. **Test save:**
   ```bash
   # Press Ctrl+S
   # Status shows "✓ Saved 1 edit(s)"
   # Close and reopen PDF → changes persisted
   ```

6. **Verify incremental update:**
   ```bash
   # Check file size grew (incremental update appended)
   ls -lh test.pdf
   # Verify still valid PDF
   cargo run -p zpdf-cli -- info test.pdf
   ```

## Performance Notes

- Annotations loaded once per page (`load_page_annotations()`)
- Mouse move updates only in-memory `current_rect` (O(1) update)
- Rendering selection border: one extra div per selected annotation
- Save operation: single file write, no re-parsing entire document
- Edit buffer uses `HashMap` for O(1) lookup/update by ObjectId

## Conclusion

This implementation provides a complete reference for building interactive PDF editors with the `zpdf-writer` API. The GUI demonstrates:

✅ Parsing annotations from existing PDFs  
✅ Hit testing and selection  
✅ Interactive dragging with coordinate conversion  
✅ Buffered editing with last-write-wins semantics  
✅ Incremental update writing  
✅ Atomic file replacement  

Developers can use this as a starting point for more advanced editors with resize, rotate, properties panels, and multi-select.
