# zpdf

[![CI](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/Xero-Team/zpdf/actions/workflows/ci.yml)
[![Release](https://github.com/Xero-Team/zpdf/actions/workflows/release.yml/badge.svg)](https://github.com/Xero-Team/zpdf/actions/workflows/release.yml)
[![Publish (crates.io)](https://github.com/Xero-Team/zpdf/actions/workflows/publish.yml/badge.svg)](https://github.com/Xero-Team/zpdf/actions/workflows/publish.yml)
[![crates.io](https://img.shields.io/crates/v/zpdf.svg)](https://crates.io/crates/zpdf)
[![docs.rs](https://img.shields.io/docsrs/zpdf)](https://docs.rs/zpdf)

Pure-Rust PDF parsing and rendering, with interchangeable CPU (tiny-skia) and
GPU (wgpu) renderers whose output matches within <1% of pixels.

## Features

- **Pure Rust** — zero C/C++ dependencies, fully safe.
- **PDF parsing** — header, traditional xref + xref/object streams + hybrid
  `/XRefStm`, trailer chains, lazy xref repair, object model, stream filters
  (Flate / LZW / ASCII85 / ASCIIHex / RunLength / DCT / CCITT G3-G4 +
  predictors) with corrupt-stream salvage.
- **Malformed-input robustness** — opens corrupt, headerless, or garbage-tail
  files via full-file object-scan recovery (catalog inside an `/ObjStm`,
  page-tree synthesis from `/Type /Page` scan, byte-flipped `/Type` tolerance,
  lenient header/dict parsing). Never panics or hangs on adversarial input:
  path/raster/clip budgets plus interpret/render time backstops degrade to a
  partial render instead. Recent hardening: predictor overflow protection,
  safe ObjStm parsing, mesh shading NaN validation, font cache LRU eviction
  (256-font limit) to prevent memory exhaustion attacks.
- **Performance** — CMYK/ColorSpace caching, zero-copy knockout rendering,
  in-place dash pattern optimization, and zero-copy PNG saving deliver 2–3×
  faster rendering on typical workloads (layered PDFs, technical drawings,
  repeated colors).
- **Encryption** — RC4 (40/128-bit) and AES-128 / AES-256 (V5 R5/R6) standard
  security handler with crypt filters; opens with the user, owner, or empty
  password (`open_with_password`, CLI `--password`).
- **Content interpretation** — graphics state, paths, clipping, text (incl.
  render modes and rise), inline & XObject images, Form XObjects (full
  resources, `/BBox` clip), axial/radial shadings, shading patterns, all 16
  blend modes, dash patterns.
- **Color** — DeviceGray/RGB/CMYK, ICCBased (`/N`), Indexed, Lab,
  Separation/DeviceN via a full PDF function evaluator (types 0/2/3/4).
- **Fonts** — embedded TrueType, Type1, Type1C/CFF, CID/Type0 (Identity-H,
  `/W`, `/CIDToGIDMap` streams), Type3, the standard-14 fonts; encodings +
  `/Differences`; Quartz-subset recovery; `/ToUnicode` text extraction.
- **Images** — 1/2/4/8/16-bpc, `/Decode`, soft masks, stencil & color-key
  masks, Indexed palettes, CMYK JPEG; bilinear sampling with box-filter
  minification.
- **Page geometry** — CropBox-aware rendering, page-tree attribute
  inheritance (`/Rotate`, `/Resources`, boxes), page rotation.
- **Annotations & forms** — `/AP` appearance streams (`/AS` states,
  Hidden/NoView); an AcroForm field model (`acro_form()`, CLI `forms`) that
  generates text/choice field appearances when the producer left none.
- **Navigation & metadata** — the document outline / bookmarks (`outline()`,
  CLI `outline`), named & explicit destinations resolved to a page + view
  (both `/Names /Dests` and the legacy `/Root /Dests`), page labels
  (`/PageLabels` → `page_labels()`, decimal/roman/letters + prefix/offset), link
  annotations resolved to their target page or URI (`page_annotations()` →
  `dest`/`uri`, CLI `links`), XMP `/Metadata` via a hardened, entity-expansion-safe
  scrape (`xmp_metadata()`), and the `/Info` dictionary (`info()`). `zpdf info`
  surfaces the `/Info`, XMP, outline, and page-label data.
- **Logical structure / Tagged PDF** — the `/StructTreeRoot` structure tree
  (`struct_tree()`, CLI `struct`) read into a navigable model of structure
  elements with their roles (`/S` resolved through `/RoleMap` to standard types
  like `H1`/`Table`/`Figure`), accessibility text (`/Alt`/`/ActualText`),
  language, titles, page associations, and marked-content (MCID) / object
  (`/OBJR`) kids; plus tagged-ness detection (`is_tagged()`, `/MarkInfo`).
  Text extraction can follow the structure tree's **logical reading order** with
  `/ActualText` / `/Alt` substitution (`struct_ordered_text()`, CLI
  `text --struct`) — the interpreter binds each text run to its `/MCID` — instead
  of the geometric XY-cut default.
- **CPU rendering** — tiny-skia backend, PNG output at any DPI.
- **GPU rendering** — wgpu backend (fills, strokes, clips, text, images, blend
  groups); matches the CPU renderer within <1% pixels.
- **Tooling** — CLI (`info`/`render`/`text`/`tables`/`forms`/`outline`/`links`/`struct`/`attachments`/`compare`/`dump`/`debug-stream`),
  an interactive winit viewer example, and a native GPUI desktop reader
  (`zpdf-viewer-gpui`).

## Documentation

- **[docs/user-guide.md](docs/user-guide.md)** — the `zpdf` command-line tool.
- **[docs/library.md](docs/library.md)** — using zpdf as a Rust library + architecture.
- **[docs/CHANGELOG.md](docs/CHANGELOG.md)** — release notes.
- **[ROADMAP.md](ROADMAP.md)** — development plan.

## Install

**Command-line tool** — `cargo install` builds the `zpdf` binary:

```bash
cargo install zpdf-cli                  # CPU rendering (default)
cargo install zpdf-cli --features gpu   # + the wgpu GPU backend
```

```bash
zpdf info document.pdf
zpdf render document.pdf -p 1 -o out.png --dpi 150
```

**Library** — add the `zpdf` facade crate to your project:

```bash
cargo add zpdf                          # CPU rendering (default)
cargo add zpdf --features gpu-render    # + the wgpu GPU backend
```

Published on [crates.io](https://crates.io/crates/zpdf) · API docs on
[docs.rs](https://docs.rs/zpdf).

## Quick start

Run from a checkout with `cargo run` (or drop the `cargo run -p zpdf-cli --`
prefix once the CLI is installed):

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

# Native desktop reader (GPUI: page list, zoom, fit-width, keyboard nav)
cargo run -p zpdf-viewer-gpui -- document.pdf
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

let display_list = ContentInterpreter::new(page.effective_box()) // CropBox ∩ MediaBox
    .with_page_rotation(page.rotate)
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

14-crate workspace with a strict one-direction dependency flow. **Render backends
depend only on `zpdf-display-list`, never on the parser** — parsing and rendering stay
fully decoupled.

```
zpdf-core            Shared types: ObjectId, PdfObject, Matrix, Rect, Error, ParseLimits
  ├─ zpdf-parser     Lexer, xref/trailer, object & stream decoding, filters
  │   └─ zpdf-document   Catalog, page tree, resource inheritance, font loading
  │       └─ zpdf-content   Content-stream interpreter → DisplayList
  ├─ zpdf-font       Type1 / TrueType / CID / Type3 fonts, CMap, encodings
  ├─ zpdf-image      JPEG / Flate / CCITT / masks / palettes → RGBA
  ├─ zpdf-color      Device / Indexed / Lab color + PDF function evaluator
  ├─ zpdf-display-list   Flat RenderCommand sequence (the backend contract)
  ├─ zpdf-render          RenderBackend trait
  │   ├─ zpdf-render-cpu   tiny-skia backend
  │   └─ zpdf-render-wgpu  wgpu backend (+ winit viewer example)
  ├─ zpdf-cli        CLI tool
  ├─ zpdf-viewer-gpui  Native desktop reader (GPUI; depends on the facade)
  └─ zpdf            Facade crate (re-exports; feature-gates cpu / gpu)
```

## Supported PDF features

| Feature | Status |
| --- | --- |
| PDF 1.0–2.0 header | ✅ |
| Traditional xref + xref/object streams + hybrid `/XRefStm` | ✅ |
| Incremental update (`/Prev`) chains, lazy xref repair | ✅ |
| Corrupt/headerless recovery (object scan, catalog-in-`/ObjStm`, page-tree synthesis) | ✅ |
| Adversarial-input safety (no panics, no hangs; budget-bounded partial render) | ✅ |
| Flate / LZW / ASCII85 / ASCIIHex / RunLength + predictors | ✅ with corrupt-stream salvage |
| DCTDecode (JPEG, incl. CMYK) / CCITTFaxDecode (G3/G4) | ✅ |
| Encryption: RC4 40/128, AES-128, AES-256 (R5/R6), crypt filters | ✅ user / owner / empty password |
| Page tree + attribute inheritance (`/Rotate`, `/Resources`, boxes) | ✅ |
| CropBox-aware rendering + page rotation | ✅ |
| Graphics state, paths, painting, clipping, dash patterns | ✅ |
| DeviceGray / DeviceRGB / DeviceCMYK / ICCBased (`/N`) | ✅ |
| Indexed / Lab / Separation / DeviceN (tint transforms) | ✅ |
| PDF functions (sampled / exponential / stitching / PostScript) | ✅ |
| Axial & radial shadings (`sh` + shading patterns) | ✅ |
| Mesh shadings: free-form/lattice Gouraud (type 4/5), Coons/tensor patches (type 6/7) | ✅ |
| Tiling patterns (PatternType 1, colored/uncolored cell replication) | ✅ |
| 16 blend modes (`/BM`) | ✅ both backends |
| Text + text state operators, render modes, rise | ✅ (text-as-clip approximated) |
| Type3 / TrueType / Type1 / Type1C / CID-Type0 / standard-14 fonts | ✅ |
| Encodings + `/Differences`, `/ToUnicode` extraction | ✅ |
| `/CIDToGIDMap` streams, OpenType-wrapped CID CFF | ✅ |
| Inline & XObject images: 1–16 bpc, `/Decode`, SMask, `/Mask`, palettes | ✅ |
| Form XObjects (full resources, `/BBox` clip, recursion guards) | ✅ |
| CPU rendering (PNG) | ✅ |
| GPU rendering (wgpu) | ✅ |
| ExtGState soft masks (`/SMask`), transparency groups | ✅ (knockout + non-isolated) |
| ICC color profiles (real color management, `moxcms`) | ✅ |
| Annotation appearance streams (`/AP`, `/AS`, Hidden/NoView) | ✅ |
| Interactive forms (AcroForm): field model + generated text/choice appearances | ✅ |
| Non-embedded font fallback (incl. CJK via system fonts) | ✅ |
| JBIG2 / JPX (JPEG 2000) filters | ✅ |
| Optional content groups / layers (`/OCG`, `/OCMD`, `/VE`) | ✅ |
| Predefined + embedded CMaps, vertical writing (`WMode 1`) | ✅ |

## Dependencies

All pure Rust:

| Crate | Purpose |
| --- | --- |
| `ttf-parser` | TrueType / OpenType / CFF font parsing |
| `tiny-skia` | CPU 2D rasterization |
| `flate2` (`rust_backend`) | FlateDecode |
| `zune-jpeg` | JPEG (DCTDecode) |
| `aes` + `cbc` + `sha2` | AES decryption (RustCrypto) |
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
- **Phase 4** — Advanced features — done (encryption incl. AES + user/owner
  passwords, shadings + mesh shadings, blend modes, spot color, CropBox/rotation,
  tiling-pattern cells, soft masks & transparency groups, annotation appearance
  streams, interactive forms (AcroForm), optional content, ICC color management,
  JBIG2 + JPEG 2000, system-font fallback, composite-font CMaps + vertical
  writing)
- **Robustness** — corrupt/adversarial-corpus pass: opens 426/618 of a
  malformed-PDF corpus (from 166), zero render panics, zero timeouts/hangs

## License

MIT
