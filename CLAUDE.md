# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                          # build all crates (default: cpu-render)
cargo build --features gpu-render    # include wgpu GPU backend
cargo test                           # run all tests
cargo test -p zpdf-parser            # test a single crate
cargo clippy --workspace             # lint all crates
cargo run -p zpdf-cli -- render <file.pdf> -p 1 -o out.png --dpi 150
cargo run -p zpdf-cli -- info <file.pdf>
cargo run -p zpdf-cli -- dump <file.pdf> <obj-num> <gen-num>
cargo run -p zpdf-cli -- debug-stream <file.pdf> <obj-num> <gen-num>
```

Features: `cpu-render` (default, tiny-skia), `gpu-render` (wgpu). Set on the root `zpdf` crate.

## Architecture

14-crate workspace. Strict one-direction dependency flow — **render backends never depend on the parser**.

```
PDF bytes
  → zpdf-parser     (lexer, xref incl. /XRefStm + lazy repair, object/stream decoding, filters, RC4/AES decryption)
  → zpdf-document   (catalog, page tree + attribute inheritance, effective_box/CropBox, font loading)
  → zpdf-content    (content stream tokenizer → operator interpreter; shading.rs evaluates axial/radial gradients)
  → zpdf-display-list (flat RenderCommand sequence)
  → zpdf-render-cpu | zpdf-render-wgpu  (implements RenderBackend trait from zpdf-render)
```

Supporting crates feed into zpdf-content: **zpdf-font** (Type1/TrueType/CID, CMap, encoding), **zpdf-image** (JPEG/Flate/CCITT/masks/palettes → RGBA), **zpdf-color** (device/Indexed/Lab conversion + the PDF function evaluator in `function.rs` — types 0/2/3/4, used by tint transforms and shadings).

**zpdf-core** provides shared types used everywhere: `ObjectId`, `PdfObject`, `Matrix`, `Rect`, `Error`, `ParseLimits`.

**zpdf** is the public facade crate — re-exports all APIs, feature-gates `cpu`/`gpu` modules.

**zpdf-cli** is the binary crate with subcommands: `info`, `dump`, `render`, `text`, `compare`, `debug-stream`.

**zpdf-viewer-gpui** is a standalone native desktop reader built on Zed's GPUI (`publish = false`); it depends on the `zpdf` facade with `gpu-render` and renders pages through the wgpu backend. Not part of the parsing/rendering dependency chain. (`zpdf-render-wgpu` also ships a lighter winit-based `viewer` example.)

## Key Design Constraints

- **Pure Rust, zero C/C++ deps.** flate2 uses `rust_backend`; image uses only `png` feature; crypto via RustCrypto (aes/cbc/sha2). This is intentional — do not add C dependencies.
- **ParseLimits** (zpdf-core) enforces safety limits at parse time: max recursion depth, stream size, image pixels, operator count. Always respect these when adding parsing code.
- **PDF coordinate system:** origin bottom-left, Y+ upward. Backends honor the page rect origin and flip Y: `((x - rect.x0) * scale, (rect.y1 - y) * scale)`. Scale = DPI / 72.0; raster dims use ceil. Pages render at `PdfPage::effective_box()` (CropBox ∩ MediaBox), with `/Rotate` baked in by `ContentInterpreter::with_page_rotation`.
- **DisplayList is flat** — no nesting. Clip/blend grouping uses Push/Pop pairs. Backends only consume `Vec<RenderCommand>`, never PDF objects.
- **Lazy parsing with caching** — objects decoded on-demand via xref offset, cached in ObjectStore. Font/image caches are per-page.

## Code Patterns

- Error handling: `thiserror` derive, all functions return `Result<T>` (crate-local alias).
- Logging: `tracing` crate (`warn!`, `debug!`), not `println!`.
- PDF lexer: manual byte-by-byte tokenizer in zpdf-parser (`Lexer<'a>` over `&[u8]`).
- Content interpretation: operand stack (`Vec<PdfObject>`) + graphics state stack (`Vec<GraphicsState>`) — standard PDF stack machine.
- File data sharing: `Arc<[u8]>` for zero-copy access across crates.

## Design Documents

- **DESIGN.md** — comprehensive architecture spec (Chinese), covers type definitions, dependency topology, safety design, filter pipeline, text model.
- **ROADMAP.md** — 4-phase development plan (Chinese) with milestone checklists. Phase 1-2 mostly complete, Phase 3 (wgpu) architecture ready, Phase 4 (ICC, blend modes, encryption) planned.
