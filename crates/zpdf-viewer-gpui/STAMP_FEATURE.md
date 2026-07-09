# zpdf-viewer-gpui — Stamp/Watermark Feature

## Overview

The zpdf-viewer-gpui now includes **stamp/watermark functionality** using the zpdf-writer incremental update system. This proof-of-concept demonstrates live PDF editing within the viewer.

## Features Added

### Quick Stamp Buttons
Three predefined stamps accessible via keyboard shortcuts or toolbar buttons:

1. **Watermark** (Key: `W`) 
   - Text: "WATERMARK"
   - Size: 48pt
   - Color: Gray (0.7, 0.7, 0.7)

2. **Draft** (Key: `D`)
   - Text: "DRAFT"
   - Size: 72pt
   - Color: Red (1.0, 0.0, 0.0)

3. **Confidential** (Key: `C`)
   - Text: "CONFIDENTIAL"
   - Size: 48pt
   - Color: Red (1.0, 0.0, 0.0)

### How It Works

1. **Click a stamp button** or press the keyboard shortcut
2. The stamp is **immediately applied** to the current page center
3. The PDF is **saved in-place** via incremental update (preserves original + appends changes)
4. The viewer **reloads the page** to show the updated content
5. A success message appears in the status area

### Technical Details

**Architecture**:
```
User clicks button
  ↓
Viewer::add_stamp()
  ↓
Read original PDF bytes
  ↓
IncrementalWriter::new()
  ↓
writer.stamp_page(page_idx, [StampItem::Text {...}])
  ↓
Write to .pdf.tmp
  ↓
Atomic rename to original path
  ↓
Clear page cache
  ↓
Notify UI → page re-renders with stamp
```

**Positioning**: Stamps are centered on the page using:
```rust
let x = (page.width / 2.0) - 100.0; // Rough horizontal center
let y = page.height / 2.0;          // Vertical center
```

**Font**: Uses Helvetica-Bold (Standard 14 font, no embedding needed)

**Safety**: 
- Atomic rename prevents corruption
- Original PDF is preserved in incremental update
- Errors are caught and displayed in status area

## Usage

### Running the Viewer

```bash
# Open a PDF
cargo run -p zpdf-viewer-gpui -- tests/corpus/clip.pdf
```

### Adding Stamps

**Method 1: Keyboard shortcuts**
- Press `W` for watermark
- Press `D` for draft stamp
- Press `C` for confidential stamp

**Method 2: Toolbar buttons**
- Click the "💧 Water" button
- Click the "📝 Draft" button
- Click the "🔒 Conf" button

### Navigation

Existing keyboard shortcuts still work:
- `J`/`K` or arrow keys: next/previous page
- `+`/`-`: zoom in/out
- `F`: fit to width
- `0`: actual size
- `G`/Shift-G: first/last page

## Implementation Files

**Modified**:
- `src/actions.rs`: Added `AddWatermark`, `AddDraftStamp`, `AddConfidentialStamp` actions
- `src/app.rs`: Registered keyboard bindings (`W`, `D`, `C`)
- `src/viewer.rs`: 
  - Added `add_stamp()` method (generic stamping logic)
  - Added `add_watermark()`, `add_draft_stamp()`, `add_confidential_stamp()` action handlers
  - Added toolbar buttons in render function
  - Registered action listeners

**Lines of code**: ~120 lines added

## Limitations

1. **Fixed positioning**: Stamps always go to page center (no drag-and-drop yet)
2. **Fixed styles**: Only 3 predefined stamps (no custom text input yet)
3. **No undo**: Each stamp is immediately saved (would need edit history)
4. **No preview**: Stamp is applied directly without preview overlay
5. **Single page**: Stamps one page at a time (no batch operations)

## Future Enhancements

Possible improvements:
- **Click-to-place**: Click on page to set stamp position
- **Custom text dialog**: Enter arbitrary stamp text
- **Style picker**: Choose font, size, color
- **Preview overlay**: Show stamp before applying
- **Undo/redo**: Buffer edits in memory before saving
- **Batch mode**: Apply stamp to multiple pages
- **Opacity control**: Semi-transparent watermarks
- **Rotation**: Diagonal "CONFIDENTIAL" stamps

## Example Use Cases

### Document Workflow
1. Review a PDF in the viewer
2. Press `D` to mark it as DRAFT
3. Send to reviewer
4. After approval, press `C` for CONFIDENTIAL
5. Final version is archived with stamps

### Quick Watermarking
1. Open a PDF to share externally
2. Press `W` on each page
3. Watermarked version is ready to send

## Comparison with CLI

**Viewer advantages**:
- Visual confirmation (see stamp immediately)
- Page-by-page control (navigate and stamp selectively)
- Quick iteration (keyboard shortcuts)

**CLI advantages**:
- Batch processing (stamp all pages at once)
- Scriptable/automatable
- Custom text/position without code changes

Both use the same `zpdf-writer` backend!

## Testing

Try it:
```bash
# Build and run
cargo build -p zpdf-viewer-gpui
cargo run -p zpdf-viewer-gpui -- tests/corpus/clip.pdf

# In viewer:
# 1. Press 'D' to add DRAFT stamp
# 2. Press 'j' to go to next page
# 3. Press 'C' to add CONFIDENTIAL stamp
# 4. Press 'k' to go back and see the DRAFT stamp
```

## Status Messages

The viewer shows success/error messages:
- ✅ `✓ Added 'DRAFT' stamp to page 1`
- ❌ `Failed to stamp: <error message>`

Messages appear in the top toolbar area where ink annotation status is shown.

## Code Snippet

Core stamping logic:
```rust
fn add_stamp(&mut self, text: &str, size: f64, color: (f64, f64, f64), cx: &mut Context<Self>) {
    // Read original PDF
    let original_bytes = std::fs::read(&self.document.summary.path)?;
    
    // Create writer
    let mut writer = IncrementalWriter::new(original_bytes)?;
    
    // Position in center
    let page = &self.document.summary.pages[self.current_page];
    let x = (page.width / 2.0) - 100.0;
    let y = page.height / 2.0;
    
    // Create stamp
    let stamp = zpdf::StampItem::Text {
        text: text.to_string(),
        x, y,
        font: "Helvetica-Bold".to_string(),
        size, color,
    };
    
    // Apply and save
    writer.stamp_page(self.current_page, &[stamp])?;
    writer.write(&mut File::create(temp_path)?)?;
    std::fs::rename(temp_path, &original_path)?;
    
    // Reload
    self.page_cache.clear();
    cx.notify();
}
```

## Conclusion

This proof-of-concept demonstrates:
- ✅ Integration of zpdf-writer with the viewer
- ✅ Live PDF editing with immediate visual feedback
- ✅ Keyboard shortcuts and toolbar buttons
- ✅ Error handling and status messages
- ✅ Atomic file updates (safe concurrent editing)

The implementation is minimal (~120 lines) but functional, and provides a foundation for more advanced editing features like custom text input, drag-and-drop positioning, and multi-page operations.
