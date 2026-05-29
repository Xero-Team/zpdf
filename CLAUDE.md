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

13-crate workspace. Strict one-direction dependency flow — **render backends never depend on the parser**.

```
PDF bytes
  → zpdf-parser     (lexer, xref, object/stream decoding, filters)
  → zpdf-document   (catalog, page tree, resource inheritance)
  → zpdf-content    (content stream tokenizer → operator interpreter)
  → zpdf-display-list (flat RenderCommand sequence)
  → zpdf-render-cpu | zpdf-render-wgpu  (implements RenderBackend trait from zpdf-render)
```

Supporting crates feed into zpdf-content: **zpdf-font** (Type1/TrueType/CID, CMap, encoding), **zpdf-image** (JPEG/Flate/masks → RGBA), **zpdf-color** (DeviceGray/RGB/CMYK/Indexed/Lab).

**zpdf-core** provides shared types used everywhere: `ObjectId`, `PdfObject`, `Matrix`, `Rect`, `Error`, `ParseLimits`.

**zpdf** is the public facade crate — re-exports all APIs, feature-gates `cpu`/`gpu` modules.

**zpdf-cli** is the binary crate with subcommands: `info`, `dump`, `render`, `debug-stream`.

## Key Design Constraints

- **Pure Rust, zero C/C++ deps.** flate2 uses `rust_backend`; image uses only `png` feature. This is intentional — do not add C dependencies.
- **ParseLimits** (zpdf-core) enforces safety limits at parse time: max recursion depth, stream size, image pixels, operator count. Always respect these when adding parsing code.
- **PDF coordinate system:** origin bottom-left, Y+ upward. CPU renderer flips Y: `(page_height - y) * scale`. Scale = DPI / 72.0.
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
