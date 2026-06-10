# zpdf Library Guide

zpdf is a pure-Rust PDF toolkit usable as a library. The `zpdf` facade crate
re-exports everything you need: document parsing, content interpretation, and the
CPU/GPU rendering backends.

For the command-line tool, see [user-guide.md](user-guide.md).

## Adding zpdf

```toml
[dependencies]
zpdf = { path = "crates/zpdf" }            # or a git / version dependency
```

### Feature flags

| Feature | Default | Effect |
| --- | --- | --- |
| `cpu-render` | ✅ | Exposes `zpdf::cpu` (tiny-skia renderer). |
| `gpu-render` | — | Exposes `zpdf::gpu` (wgpu renderer). Pulls in `wgpu`, `lyon`, `pollster`. |

```toml
zpdf = { path = "crates/zpdf", features = ["gpu-render"] }      # CPU + GPU
zpdf = { path = "crates/zpdf", default-features = false, features = ["gpu-render"] } # GPU only
```

Parsing and content interpretation are always available; only the rendering backends
are feature-gated.

## The rendering pipeline

```
PDF bytes ─▶ PdfDocument ─▶ PdfPage ─▶ ContentInterpreter ─▶ DisplayList ─▶ RenderBackend ─▶ pixels
            (parse/xref)   (page tree)  (operator interp.)   (flat commands)  (cpu | gpu)
```

The `DisplayList` is a flat, backend-agnostic sequence of `RenderCommand`s. Both
backends consume the same display list, so switching between CPU and GPU is a one-line
change.

## Rendering a page to PNG

```rust
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

fn render_first_page(path: &str, out: &str) -> zpdf::Result<()> {
    let data = std::fs::read(path).map_err(zpdf::Error::Io)?;
    let doc = PdfDocument::open(data)?;

    let page = doc.page(0)?;                          // 0-based
    let mut fonts = doc.load_page_fonts(&page);       // per-page font cache
    let mut images = ImageCache::new();               // per-page image cache
    let content = doc.page_content_bytes(&page)?;     // decoded content stream

    // Interpret operators into a DisplayList. The interpreter borrows the caches
    // mutably (it may add Form-XObject fonts / decode images) and is consumed by
    // `interpret`, releasing those borrows.
    //
    // effective_box() = CropBox ∩ MediaBox (what a viewer shows); pass
    // page.rotate so /Rotate'd pages come out upright.
    let display_list = ContentInterpreter::new(page.effective_box())
        .with_page_rotation(page.rotate)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);

    // Render. scale = dpi / 72.0 — here 150 DPI.
    let scale = 150.0 / 72.0;
    let mut renderer = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images);
    let page_img = renderer.render_display_list(&display_list, scale)?;

    page_img.save_png(out)?;                           // RenderedPage::save_png
    Ok(())
}
```

`RenderedPage` exposes `width: u32`, `height: u32`, and `data: Vec<u8>` (tight RGBA8,
top-left origin) in addition to `save_png`.

## Rendering on the GPU

With `features = ["gpu-render"]`, swap the backend — everything upstream is identical:

```rust
let mut renderer = zpdf::gpu::WgpuRenderer::new()
    .with_fonts(&fonts)
    .with_images(&images);
let tex = renderer.render_display_list(&display_list, scale)?; // GpuTexture { width, height, data }
// tex.data is tight RGBA8, identical layout to RenderedPage.
```

`WgpuRenderer::new()` lazily creates a headless GPU context on first render and reuses
it across pages. `render_display_list` returns an error (`WgpuRenderError::NoAdapter`)
if no GPU adapter is available — handle it to fall back to the CPU backend.

## Extracting text

Provide a text sink to the interpreter, then reflow the collected spans:

```rust
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, TextSpan, spans_to_text};

let doc = PdfDocument::open(std::fs::read("doc.pdf").map_err(zpdf::Error::Io)?)?;
let page = doc.page(0)?;
let mut fonts = doc.load_page_fonts(&page);
let mut images = ImageCache::new();
let content = doc.page_content_bytes(&page)?;

let mut spans: Vec<TextSpan> = Vec::new();
{
    let interp = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_text_sink(&mut spans);
    let _ = interp.interpret(&content);
}
let text = spans_to_text(spans, 2.0); // line-merge tolerance (pt-relative)
println!("{text}");
```

Each `TextSpan` carries the decoded text plus its position and size, so you can build
your own layout/extraction logic instead of using `spans_to_text`.

## Inspecting documents

```rust
let doc = PdfDocument::open(bytes)?;       // decrypts RC4/AES (empty password) transparently
let (major, minor) = doc.version();        // e.g. (1, 7)
let n = doc.page_count();
let page = doc.page(i)?;
let (w, h) = (page.width(), page.height()); // MediaBox, points
let visible = page.effective_box();         // CropBox ∩ MediaBox (render rect)
let rot = page.rotate;                      // inherited /Rotate (degrees)
let annots = &page.annots;                  // /Annots object ids (not yet rendered)
let obj = doc.file().resolve(zpdf::ObjectId(4, 0))?;          // a PdfObject
let stream = doc.file().resolve_stream_data(zpdf::ObjectId(7, 0))?; // decoded bytes
```

Encrypted documents (RC4 and AES-128/256, empty user password) open and decrypt
transparently in `PdfDocument::open`; password-protected files degrade to a
warning and render blank.

## Architecture

A 13-crate workspace with a strict one-direction dependency flow. **Render backends
never depend on the parser** — they consume only `zpdf-display-list`.

```
zpdf-core            Shared types: ObjectId, PdfObject, Matrix, Rect, Error, ParseLimits
  ├─ zpdf-parser     Lexer, xref/trailer (+ hybrid /XRefStm, lazy repair),
  │   │              object & stream decoding, filters, RC4/AES decryption
  │   └─ zpdf-document   Catalog, page tree + attribute inheritance, font loading
  │       └─ zpdf-content   Content-stream tokenizer → operator interpreter,
  │                         shading evaluation (zpdf_content::shading)
  ├─ zpdf-font       Type1 / TrueType / CID fonts, CMap, encodings, glyph outlines
  ├─ zpdf-image      JPEG / Flate / CCITT / masks / palettes → RGBA
  ├─ zpdf-color      Device/Indexed/Lab color conversion + the PDF function
  │                  evaluator (zpdf_color::function, types 0/2/3/4)
  ├─ zpdf-display-list   Flat RenderCommand sequence (the backend contract)
  ├─ zpdf-render          RenderBackend trait + PageRenderInfo + dash flattening
  │   ├─ zpdf-render-cpu   tiny-skia backend  → RenderedPage
  │   └─ zpdf-render-wgpu  wgpu backend       → GpuTexture
  ├─ zpdf-cli        Binary: info / dump / render / text / compare / debug-stream
  └─ zpdf            Facade crate (re-exports; feature-gates cpu / gpu)
```

### Design constraints

- **Pure Rust, zero C/C++ dependencies.** `flate2` uses `rust_backend`; `image` uses
  only its `png` feature.
- **`ParseLimits`** (in `zpdf-core`) bounds recursion depth, stream size, image pixels,
  and operator counts at parse time.
- **PDF coordinates** are origin-bottom-left, +Y up. Renderers honor the page
  rect's origin and flip Y: `((x − rect.x0) · scale, (rect.y1 − y) · scale)`,
  where `scale = dpi / 72` — so CropBoxes and nonzero MediaBox origins render
  correctly. Raster dimensions are `ceil(rect_size · scale)`.
- **The `DisplayList` is flat** — no nesting. Clip and blend grouping use Push/Pop pairs.
- File data is shared zero-copy as `Arc<[u8]>`.

## Implementing a custom backend

Implement the `RenderBackend` trait (from `zpdf-render`); the default
`render_display_list` drives it:

```rust
use zpdf_render::{PageRenderInfo, RenderBackend};
use zpdf_display_list::RenderCommand;

pub trait RenderBackend {
    type Target;
    type Error: std::error::Error;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error>;
    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error>;
    fn end_page(&mut self) -> Result<Self::Target, Self::Error>;
    // provided: render_display_list(dl, scale) = begin_page → execute* → end_page
}
```

`RenderCommand` variants: `FillPath`, `StrokePath`, `DrawGlyphRun`, `DrawImage`,
`PushClip`/`PopClip`, `PushBlendGroup`/`PopBlendGroup`. The CPU and GPU backends are the
reference implementations; the GPU backend reproduces the CPU's pixels within <1%.

## Error handling

All fallible APIs return `zpdf::Result<T>` (alias for `Result<T, zpdf::Error>`).
`zpdf::Error` is a `thiserror` enum (`Io`, `StreamDecode`, `TypeMismatch`, `MissingKey`,
…). Rendering backends define their own error types (`CpuRenderError`,
`WgpuRenderError`) returned from `render_display_list`.
