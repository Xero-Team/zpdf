# Quick Start: Using Stamps in zpdf-viewer-gpui

## Step-by-Step Guide

### 1. Build the Viewer
```bash
cd D:/project/zpdf
cargo build -p zpdf-viewer-gpui
```

### 2. Open a PDF
```bash
cargo run -p zpdf-viewer-gpui -- tests/corpus/curves.pdf
```

### 3. Add Stamps

The viewer window will show:
```
┌─────────────────────────────────────────────────────────────┐
│ zpdf GPUI viewer                                      - □ ✕ │
├─────────────────────────────────────────────────────────────┤
│ [|<] [Prev] Page 1/1 [Next] [>|]  [-] 100% [+] [Fit width] │
│ [✏ Ink]  [💧 Water] [📝 Draft] [🔒 Conf]                   │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│                    (PDF page content)                        │
│                                                               │
├─────────────────────────────────────────────────────────────┤
│ curves.pdf • PDF-1.4                                         │
└─────────────────────────────────────────────────────────────┘
```

**Option A: Use Keyboard**
- Press `D` → Red "DRAFT" stamp appears
- Press `W` → Gray "WATERMARK" stamp appears  
- Press `C` → Red "CONFIDENTIAL" stamp appears

**Option B: Use Mouse**
- Click "📝 Draft" button → Red "DRAFT" stamp appears
- Click "💧 Water" button → Gray "WATERMARK" stamp appears
- Click "🔒 Conf" button → Red "CONFIDENTIAL" stamp appears

### 4. Verify Changes

After adding a stamp:
1. The stamp appears **immediately** on the page
2. Status message shows: `✓ Added 'DRAFT' stamp to page 1`
3. The PDF file is **updated on disk**
4. Close and reopen the file - **stamp is permanent**

### 5. Multi-Page Example

For a multi-page PDF:
```bash
# Open a larger PDF (if available)
cargo run -p zpdf-viewer-gpui -- path/to/multi-page.pdf
```

Then:
1. Press `D` → DRAFT on page 1
2. Press `j` → Go to page 2
3. Press `C` → CONFIDENTIAL on page 2
4. Press `k` → Back to page 1 (DRAFT is still there)
5. Press `shift-g` → Jump to last page
6. Press `W` → WATERMARK on last page

## Keyboard Reference Card

### Navigation
- `j` or `↓` or `space` - Next page
- `k` or `↑` - Previous page
- `g` or `home` - First page
- `shift-g` or `end` - Last page

### Zoom
- `+` or `=` - Zoom in
- `-` - Zoom out
- `0` - Actual size (100%)
- `f` - Fit to width

### Stamps (NEW!)
- `w` - Add watermark (gray)
- `d` - Add draft stamp (red)
- `c` - Add confidential stamp (red)

### Other
- `cmd-q` or `ctrl-q` - Quit

## Expected Output

### Before Stamping
```
Original PDF:
┌──────────────┐
│              │
│   Content    │
│              │
└──────────────┘
```

### After Pressing 'D' (Draft)
```
Stamped PDF:
┌──────────────┐
│              │
│   DRAFT      │  ← Red text, 72pt, centered
│   Content    │
│              │
└──────────────┘
```

### After Pressing 'W' (Watermark)
```
Double Stamped:
┌──────────────┐
│ WATERMARK    │  ← Gray text, 48pt, centered
│   DRAFT      │  ← Red text, 72pt, centered
│   Content    │
│              │
└──────────────┘
```

## Troubleshooting

### "Failed to read PDF"
- Check file path is correct
- Ensure file exists and is readable

### "Failed to create writer"
- File may be corrupted
- File may be password-protected

### "Failed to stamp"
- Disk may be full
- File may be write-protected
- PDF may have unsupported features

### Stamp not visible
- Try zooming in (`+` key)
- Navigate to correct page
- Check console logs for errors

### File not saving
- Check file permissions
- Ensure disk space available
- Try closing other apps that may have the file open

## Testing the Feature

Quick test script:
```bash
# 1. Build
cargo build -p zpdf-viewer-gpui

# 2. Make a copy to test on
cp tests/corpus/curves.pdf /tmp/test.pdf

# 3. Open copy
cargo run -p zpdf-viewer-gpui -- /tmp/test.pdf

# 4. In viewer: Press 'D'
# 5. Close viewer
# 6. Verify with CLI
cargo run -p zpdf-cli -- info /tmp/test.pdf

# 7. Reopen to see stamp
cargo run -p zpdf-viewer-gpui -- /tmp/test.pdf
```

## Advanced Usage

### Custom Workflow
```bash
# Review workflow
1. Open document
2. Review first page
3. If needs revision: Press 'D' (draft)
4. If approved: Press 'C' (confidential)
5. Navigate to next page
6. Repeat
```

### Comparison Before/After
```bash
# Keep original
cp important.pdf important-original.pdf

# Edit in viewer
cargo run -p zpdf-viewer-gpui -- important.pdf
# Press 'W' on each page

# Compare
cargo run -p zpdf-cli -- render important-original.pdf -p 1 -o before.png
cargo run -p zpdf-cli -- render important.pdf -p 1 -o after.png
# View both PNGs side by side
```

## Performance Notes

- **Stamp application**: ~100-500ms (depends on PDF size)
- **Page reload**: Instant (uses cached renders)
- **File size increase**: ~1-5KB per stamp (incremental update)
- **Memory usage**: Same as original viewer

## File Safety

✅ **Original preserved**: Incremental updates append, don't replace  
✅ **Atomic writes**: Temp file + rename prevents corruption  
✅ **Digital signatures**: Original signatures preserved (may become invalid)  
✅ **Metadata**: Original metadata retained  

## What Gets Modified

The stamp operation adds:
- New Form XObject (stamp content)
- Updated page /Contents array
- Updated page /Resources
- New xref entries
- New trailer pointing to old one

Original objects remain unchanged!

## Logging

To see detailed logs:
```bash
RUST_LOG=zpdf_viewer_gpui=info cargo run -p zpdf-viewer-gpui -- test.pdf
```

You'll see:
```
[INFO] Adding stamp 'DRAFT' to page 0
[INFO] Stamp applied successfully!
```

## Summary

🎯 **3 keyboard shortcuts** (W/D/C)  
🎯 **3 toolbar buttons**  
🎯 **Immediate visual feedback**  
🎯 **Permanent changes to PDF**  
🎯 **Safe incremental updates**  

Try it now! 🚀
