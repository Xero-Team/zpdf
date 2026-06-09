# zpdf

[![CI](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml)

Pure-Rust PDF parsing and rendering, with interchangeable CPU (tiny-skia) and
GPU (wgpu) renderers whose output matches within <1% of pixels.

## Features

- **Pure Rust** — zero C/C++ dependencies, fully safe.
- **PDF parsing** — header, traditional xref + xref/object streams, trailer chains,
  object model, stream filters (Flate / ASCII85 / ASCIIHex / RunLength + predictors).
- **Content interpretation** — graphics state, paths, clipping, text, color, inline &
  XObject images, Form XObjects.
- **Fonts** — embedded TrueType, Type1, CID/Type0 (Identity-H, `/W`), Type3, the
  standard-14 fonts; encodings + `/Differences`; `/ToUnicode` text extraction.
- **CPU rendering** — tiny-skia backend, PNG output at any DPI.
- **GPU rendering** — wgpu backend (fills, strokes, clips, text, images, blend groups);
  matches the CPU renderer within <1% pixels.
- **Tooling** — CLI (`info`/`render`/`text`/`compare`/`dump`/`debug-stream`) and an
  interactive winit viewer.

## Documentation

- **[docs/user-guide.md](docs/user-guide.md)** — the `zpdf` command-line tool.
- **[docs/library.md](docs/library.md)** — using zpdf as a Rust library + architecture.
- **[docs/CHANGELOG.md](docs/CHANGELOG.md)** — release notes.
- **[ROADMAP.md](ROADMAP.md)** — development plan.

## Quick start

```bash
# Inspect a document
cargo run -p zpdf-cli -- info document.pdf

# Render page 1 at 150 DPI (CPU)
cargo run -p zpdf-cli -- render document.pdf -p 1 -o out.png --dpi 150

# Render on the GPU (requires the `gpu` feature)
cargo run -p zpdf-cli --features gpu -- render document.pdf -p 1 -o gpu.png --backend wgpu

# Extract text, and compare two renders
cargo run -p zpdf-cli -- text document.pdf -p 1
cargo run -p zpdf-cli -- compare out.png gpu.png --out diff.png

# Interactive viewer (pan/zoom/page-flip)
cargo run -p zpdf-render-wgpu --example viewer -- document.pdf
```

## Library usage

```rust
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

let data = std::fs::read("document.pdf").map_err(zpdf::Error::Io)?;
let doc = PdfDocument::open(data)?;

let page = doc.page(0)?;                        // 0-based
let mut fonts = doc.load_page_fonts(&page);
let mut images = ImageCache::new();
let content = doc.page_content_bytes(&page)?;

let display_list = ContentInterpreter::new(page.media_box)
    .with_fonts(&mut fonts)
    .with_document(doc.file(), &page.resources)
    .with_images(&mut images)
    .interpret(&content);

let mut renderer = zpdf::cpu::CpuRenderer::new()
    .with_fonts(&fonts)
    .with_images(&images);
let page_img = renderer.render_display_list(&display_list, 150.0 / 72.0)?; // 150 DPI
page_img.save_png("out.png")?;
```

Switch `zpdf::cpu::CpuRenderer` for `zpdf::gpu::WgpuRenderer` (with `features = ["gpu-render"]`)
to render on the GPU — everything upstream is identical. See [docs/library.md](docs/library.md).

## Architecture

13-crate workspace with a strict one-direction dependency flow. **Render backends
depend only on `zpdf-display-list`, never on the parser** — parsing and rendering stay
fully decoupled.

```
zpdf-core            Shared types: ObjectId, PdfObject, Matrix, Rect, Error, ParseLimits
  ├─ zpdf-parser     Lexer, xref/trailer, object & stream decoding, filters
  │   └─ zpdf-document   Catalog, page tree, resource inheritance, font loading
  │       └─ zpdf-content   Content-stream interpreter → DisplayList
  ├─ zpdf-font       Type1 / TrueType / CID / Type3 fonts, CMap, encodings
  ├─ zpdf-image      JPEG / Flate / masks → RGBA
  ├─ zpdf-color      DeviceGray / RGB / CMYK / Indexed / Lab
  ├─ zpdf-display-list   Flat RenderCommand sequence (the backend contract)
  ├─ zpdf-render          RenderBackend trait
  │   ├─ zpdf-render-cpu   tiny-skia backend
  │   └─ zpdf-render-wgpu  wgpu backend (+ viewer example)
  ├─ zpdf-cli        CLI tool
  └─ zpdf            Facade crate (re-exports; feature-gates cpu / gpu)
```

## Supported PDF features

| Feature | Status |
| --- | --- |
| PDF 1.0–2.0 header | ✅ |
| Traditional xref + xref/object streams | ✅ |
| Incremental update (`/Prev`) chains | ✅ |
| Flate / ASCII85 / ASCIIHex / RunLength + predictors | ✅ |
| DCTDecode (JPEG) | ✅ |
| Page tree + resource inheritance | ✅ |
| Graphics state, paths, painting, clipping | ✅ |
| DeviceGray / DeviceRGB / DeviceCMYK | ✅ |
| Text + text state operators | ✅ |
| Type3 / TrueType / Type1 / CID-Type0 / standard-14 fonts | ✅ |
| Encodings + `/Differences`, `/ToUnicode` extraction | ✅ |
| Inline & XObject images, image/soft masks | ✅ |
| Form XObjects | ✅ |
| CPU rendering (PNG) | ✅ |
| GPU rendering (wgpu) | ✅ |
| 16 blend modes (GPU) | ✅ backend; not yet emitted by the interpreter |
| ICC color profiles | Planned |
| Annotations | Planned |
| Encryption | Planned |
| LZW / CCITT / JBIG2 / JPX filters | Planned |

## Dependencies

All pure Rust:

| Crate | Purpose |
| --- | --- |
| `ttf-parser` | TrueType / OpenType / CFF font parsing |
| `tiny-skia` | CPU 2D rasterization |
| `flate2` (`rust_backend`) | FlateDecode |
| `zune-jpeg` | JPEG (DCTDecode) |
| `image` | PNG I/O |
| `winnow` | Parsing helpers |
| `wgpu` + `lyon` + `pollster` | GPU rendering (`gpu-render` feature) |
| `winit` | Viewer example only (dev-dependency) |

## Development

```bash
cargo build                                   # CPU-only (default)
cargo build --features gpu-render             # include the wgpu backend
cargo test                                    # all tests
cargo test -p zpdf --features gpu-render      # + the GPU↔CPU acceptance harness
cargo clippy --workspace
```

## Roadmap

See [ROADMAP.md](ROADMAP.md).

- **Phase 1** — PDF parsing — done
- **Phase 2** — Content interpretation + CPU rendering — done
- **Phase 3** — wgpu GPU rendering — done
- **Phase 4** — Advanced features (ICC, soft masks, annotations, encryption) — planned

## License

MIT
