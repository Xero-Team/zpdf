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

18-crate workspace. Strict one-direction dependency flow — **render backends never depend on the parser**.

```
PDF bytes
  → zpdf-parser     (lexer, xref incl. /XRefStm + lazy repair, object/stream decoding, filters, RC4/AES decryption)
  → zpdf-document   (catalog, page tree + attribute inheritance, effective_box/CropBox, font loading)
  → zpdf-content    (content stream tokenizer → operator interpreter; shading.rs evaluates axial/radial gradients, mesh.rs decodes type 4–7 mesh shadings)
  → zpdf-display-list (flat RenderCommand sequence)
  → zpdf-render-cpu | zpdf-render-wgpu  (implements RenderBackend trait from zpdf-render)
```

Supporting crates feed into zpdf-content: **zpdf-font** (Type1/TrueType/CID, CMap, encoding), **zpdf-image** (JPEG/Flate/CCITT/masks/palettes → RGBA), **zpdf-color** (device/Indexed/Lab conversion + the PDF function evaluator in `function.rs` — types 0/2/3/4, used by tint transforms and shadings).

**zpdf-core** provides shared types used everywhere: `ObjectId`, `PdfObject`, `Matrix`, `Rect`, `Error`, `ParseLimits`.

**zpdf-writer** is the authoring/editing crate (depends on parser + document + content, never on renderers): `DocumentBuilder` (creation from scratch, TrueType embedding + sparse-glyf subsetting), `IncrementalWriter` (ISO §7.5.6 updates — annotations with baked /AP, forms, stamps, redaction, page ops, signing; encrypted docs via `new_with_password`), `rewrite_pdf` (GC/decrypt/compress/downsample/encrypt-on-save), `linearize_pdf` (Annex F), `append_document` (merge incl. outlines/AcroForm/OCGs).

**zpdf-pptx-export** converts DisplayList to editable PowerPoint (.pptx) — text stays editable with font/size/color/style preserved, shapes (rectangles/ellipses/lines with fill/stroke) and images render as native PowerPoint elements rather than flattening to raster. Separate crate from rendering; depends only on display-list.

**zpdf** is the public facade crate — re-exports all APIs, feature-gates `cpu`/`gpu` modules.

**zpdf-svg-export** converts a page's DisplayList to vector-faithful standalone SVG (paths/glyph outlines stay vectors, images embed as base64 PNG, clips/soft masks/blend modes map to SVG equivalents). Mirrors CPU-backend semantics; verified against the CPU oracle via resvg rasterization (`tests/fidelity.rs`). NB: never wrap content in `<g clip-path>` — in SVG that isolates blending; clips are attribute chains on painted elements.

**zpdf-wasm** is the WebAssembly crate (`wasm32-unknown-unknown` target + `wasm-bindgen`). Exposes `Pdf.open` / `render_page` / `page_text` / `page_svg` to JS (browser or Node). Pure Rust with zero C deps is what makes this build possible where native PDF libraries struggle. Wall-clock budgets disabled (bare wasm32 has no `Instant::now`); deterministic `ParseLimits` still bound adversarial inputs. `www/` contains a drop-in browser demo; `build-web.sh` compiles + generates ES-module bindings.

**zpdf-cli** is the binary crate with subcommands: read — `info`, `dump`, `render`, `text`, `search`, `convert`, `export-pptx`, `export-svg`, `tables`, `forms`, `outline`, `links`, `struct`, `signatures` (`--trust` for cert chains), `attachments`, `validate` (PDF/A-1b/2b/3b, PDF/UA-1/2), `compare`, `debug-stream`; write — `fill`, `merge`, `split`, `optimize` (`--encrypt`, `--max-image-dim`, `--linearize`, `--pdfa pdfa-1b|2b|3b`), `annotate`, `redact`, `sign`, `pages`, `set-meta`, `stamp`.

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

- **docs/architecture/DESIGN.md** — comprehensive architecture spec (Chinese), covers type definitions, dependency topology, safety design, filter pipeline, text model.
- **docs/planning/ROADMAP.md** — 4-phase development plan (Chinese) with milestone checklists. Phase 1-2 mostly complete, Phase 3 (wgpu) architecture ready, Phase 4 (ICC, blend modes, encryption) planned.

## AI Contribution Policy

When making commits or PRs on this repo, follow [AI_POLICY.md](AI_POLICY.md):
you may use AI, but the contributor must understand the change and add a short
`## Human note` in their own words (their mother tongue) at the bottom of each
commit/PR. As an AI agent, do not author the human note yourself — remind the
user to write it, and never commit or open a PR autonomously.
