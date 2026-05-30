# zpdf

[![CI](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml)

Pure Rust PDF parsing and rendering library with wgpu GPU acceleration support.

## Features

- **Pure Rust** — zero C/C++ dependencies, fully safe
- **PDF parsing** — header, xref, trailer, object model, stream filters (Flate/ASCII85/AsciiHex/RunLength)
- **Content stream interpretation** — graphics state, path construction, text positioning, color operations
- **Font rendering** — Type3 glyph outlines (CJK), embedded TrueType (ttf-parser), CID/Type0 fonts
- **CPU rendering** — tiny-skia backend outputs PNG at arbitrary DPI
- **GPU rendering** — wgpu backend (Phase 3, architecture ready)

## Quick Start

```bash
# Show PDF metadata
cargo run -p zpdf-cli -- info document.pdf

# Render page 1 at 150 DPI
cargo run -p zpdf-cli -- render document.pdf -p 1 -o output.png --dpi 150

# Dump a specific PDF object
cargo run -p zpdf-cli -- dump document.pdf 4 0
```

## Library Usage

```rust
use zpdf::{PdfDocument, RenderBackend};

// Open and inspect
let data = std::fs::read("document.pdf")?;
let doc = PdfDocument::open(data)?;
println!("Pages: {}", doc.page_count());

// Render a page
let page = doc.page(0)?;
let font_cache = doc.load_page_fonts(&page);
let content = doc.page_content_bytes(&page)?;

let interpreter = zpdf::ContentInterpreter::new(page.media_box)
    .with_fonts(&font_cache);
let display_list = interpreter.interpret(&content);

let mut renderer = zpdf::cpu::CpuRenderer::new()
    .with_fonts(&font_cache);
let rendered = renderer.render_display_list(&display_list, 2.0)?; // 144 DPI
rendered.save_png("output.png")?;
```

## Architecture

```
zpdf-core           Base types: PdfObject, Matrix, Rect, Error
  │
  ├─ zpdf-parser     PDF file parser: lexer, xref, trailer, stream filters
  │    │
  │    └─ zpdf-document   Document model: catalog, page tree, font loading
  │         │
  │         └─ zpdf-content   Content stream interpreter → DisplayList
  │
  ├─ zpdf-font       Font engine: Type3, TrueType, CID (ttf-parser)
  ├─ zpdf-image      Image decoding (JPEG, Flate)
  ├─ zpdf-color      Color spaces (RGB, CMYK, Gray)
  │
  ├─ zpdf-display-list   Render commands: paths, glyphs, images, clips
  │
  ├─ zpdf-render     Backend-agnostic RenderBackend trait
  │    ├─ zpdf-render-cpu    CPU renderer (tiny-skia)
  │    └─ zpdf-render-wgpu   GPU renderer (wgpu, Phase 3)
  │
  ├─ zpdf-cli        CLI tool: info, dump, render
  └─ zpdf            Facade crate, re-exports everything
```

Key design constraint: render backends depend only on `zpdf-display-list`, never on `zpdf-parser`. This keeps parsing and rendering fully decoupled.

## Supported PDF Features

| Feature                                      | Status  |
| -------------------------------------------- | ------- |
| PDF 1.0–2.0 header                          | Done    |
| Traditional xref table                       | Done    |
| Xref streams (PDF 1.5+)                      | Planned |
| FlateDecode / ASCII85 / AsciiHex / RunLength | Done    |
| DCTDecode (JPEG)                             | Planned |
| Page tree traversal                          | Done    |
| Resource inheritance                         | Done    |
| Graphics state (q/Q/cm/w/J/j/M/d)            | Done    |
| Path ops (m/l/c/v/y/h/re)                    | Done    |
| Path painting (S/s/f/F/f\*/B/B\*/b/b\*/n)    | Done    |
| Clipping (W/W\*)                             | Done    |
| DeviceGray / DeviceRGB / DeviceCMYK          | Done    |
| Text (BT/ET/Tf/Td/TD/Tm/T\*/Tj/TJ/'/")       | Done    |
| Text state (Tc/Tw/Tz/TL/Ts/Tr)               | Done    |
| Type3 fonts (CharProcs glyph streams)        | Done    |
| TrueType embedded fonts                      | Done    |
| CID/Type0 fonts (Identity-H, /W widths)      | Done    |
| Image XObject (Do)                           | Planned |
| Form XObject                                 | Planned |
| ExtGState (transparency)                     | Planned |
| ICC color profiles                           | Planned |
| Blend modes                                  | Planned |
| Annotations                                  | Planned |
| Encryption                                   | Planned |
| ToUnicode text extraction                    | Planned |

## Dependencies

All pure Rust:

| Crate                 | Purpose                        |
| --------------------- | ------------------------------ |
| ttf-parser            | TrueType/OpenType font parsing |
| tiny-skia             | CPU 2D rasterization           |
| flate2 (rust_backend) | FlateDecode stream filter      |
| image                 | PNG output                     |
| winnow                | Parser combinators             |
| wgpu                  | GPU rendering (Phase 3)        |
| lyon                  | Path tessellation for GPU      |

## Development

```bash
# Build everything
cargo build

# Run tests
cargo test

# Render a test PDF
cargo run -p zpdf-cli -- render tests/your-file.pdf -p 1 -o output.png --dpi 200
```

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the full development plan.

- **Phase 1** — PDF parsing (done)
- **Phase 2** — Content stream + CPU rendering (in progress)
- **Phase 3** — wgpu GPU rendering
- **Phase 4** — Advanced features (ICC, blend modes, encryption)

## License

MIT
