# zpdf User Guide

`zpdf` is a pure-Rust PDF parser and renderer. This guide covers the `zpdf`
command-line tool: rendering pages to PNG (on CPU or GPU), extracting text,
inspecting PDF internals, and comparing renders.

For using zpdf as a Rust library, see [library.md](library.md).

## Installation

zpdf is a Cargo workspace; the CLI binary is named `zpdf` (crate `zpdf-cli`).

```bash
git clone <repo> && cd zpdf

# Build the CLI (CPU rendering only — the default)
cargo build -p zpdf-cli --release

# Build with the GPU backend enabled (adds the wgpu renderer)
cargo build -p zpdf-cli --release --features gpu
```

The release binary is at `target/release/zpdf`. The examples below use
`cargo run -p zpdf-cli --` for convenience; substitute the built binary in real use.

## Commands

| Command | Purpose |
| --- | --- |
| `info` | Print version, page count, and per-page size/rotation. |
| `render` | Render a page to a PNG (CPU or GPU). |
| `text` | Extract text from a page (or all pages). |
| `compare` | Pixel-diff two PNGs and report difference metrics. |
| `dump` | Print a resolved PDF object. |
| `debug-stream` | Print a decoded stream object's bytes. |

### `info` — inspect a document

```bash
cargo run -p zpdf-cli -- info document.pdf
```

```
File: document.pdf
Version: PDF-1.7
Pages: 16
  Page 1: 612 x 792 pt (rotate: 0)
  Page 2: 612 x 792 pt (rotate: 0)
  ...
```

### `render` — render a page to PNG

```bash
cargo run -p zpdf-cli -- render document.pdf -p 1 -o out.png --dpi 150
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `-p <page>` | `1` | 1-based page number. |
| `-o <file>` | `output.png` | Output PNG path. |
| `--dpi <n>` | `150` | Render resolution. Pixel size = `ceil(page_pt × dpi / 72)`. |
| `--backend [cpu\|wgpu]` | `cpu` | Renderer. `wgpu` requires `--features gpu`. |

Pages render at their **effective box** (CropBox ∩ MediaBox — what desktop
viewers show, e.g. a trimmed scan rather than the full sheet) and honor the
page's `/Rotate` entry, including values inherited from the page tree.
Encrypted documents (RC4 and AES-128/256 with an empty user password — the
common "owner-locked" case) open and render transparently.

GPU rendering (requires a GPU/`gpu` feature):

```bash
cargo run -p zpdf-cli --features gpu -- render document.pdf -p 1 -o gpu.png --backend wgpu
```

The two backends produce visually identical output (within anti-aliasing tolerance).
`--backend wgpu` without `--features gpu` exits with an error; an unknown backend name
exits with code 2.

### `text` — extract text

```bash
cargo run -p zpdf-cli -- text document.pdf -p 1     # one page
cargo run -p zpdf-cli -- text document.pdf --all    # every page
```

Text is reconstructed from the page's fonts using `/ToUnicode` (when present) and the
font encoding, grouped into lines and ordered left-to-right. Output goes to stdout.

### `compare` — pixel-diff two PNGs

The acceptance tool for checking two renders against each other (e.g. CPU vs GPU).

```bash
cargo run -p zpdf-cli -- compare cpu.png gpu.png --threshold 16 --out diff.png
```

```
Compare: cpu.png  vs  gpu.png
  Size: 1274x1649 (2100826 px)
  Differing pixels (>16/channel): 7736 (0.368%)
  MAE: 0.164/255    RMSE: 2.377/255    Max channel diff: 144/255
  Diff heatmap: diff.png
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--threshold <0–255>` | `16` | A pixel "differs" if its max R/G/B channel delta exceeds this. |
| `--out <file>` | — | Write a heat-map PNG: differing pixels glow red over a dimmed image. |

A *differing pixels* percentage under ~1% with low MAE means the renders agree (the
residual is anti-aliasing on edges). A dimension mismatch exits with code 2.

### `dump` and `debug-stream` — inspect internals

```bash
# Print object 4, generation 0 (resolved, references followed)
cargo run -p zpdf-cli -- dump document.pdf 4 0

# Print a stream object's decoded bytes (after filter decoding)
cargo run -p zpdf-cli -- debug-stream document.pdf 7 0
```

## Window viewers

### `simple-viewer`

A windowed viewer (winit + wgpu) ships as an example of the GPU backend:

```bash
cargo run -p zpdf-render-wgpu --example simple-viewer -- document.pdf
```

| Input | Action |
| --- | --- |
| Mouse wheel / `+` / `-` | Zoom |
| `W` `A` `S` `D` | Pan |
| `PageUp` / `PageDown` (or `←` / `→`) | Previous / next page |
| `0` | Reset zoom and pan |
| `Esc` | Quit |

The viewer renders each page on the GPU and blits it to the window; the title bar shows
the current page and zoom. It requires a working GPU adapter.

### `zpdf-viewer-gpui`

An experimental GPUI desktop frontend is also available:

```bash
cargo run -p zpdf-viewer-gpui -- document.pdf
```

This version rasterizes bitmap previews for pages and provides basic page
navigation plus zoom controls. It is still intentionally small in scope: no
search, annotations, or advanced continuous-scroll behavior yet.

## DPI, sizes, and coordinates

PDF pages are measured in points (1 pt = 1/72 inch). The rendered pixel dimensions are
`ceil(page_points × dpi / 72)` of the page's effective (cropped) box — matching
what pdfium/Chromium produces. For a US-Letter page (612 × 792 pt):

| DPI | Pixels |
| --- | --- |
| 72 | 612 × 792 |
| 150 | 1275 × 1650 |
| 300 | 2550 × 3300 |

## Supported content

Rendering covers: vector paths (fill/stroke, non-zero & even-odd, all caps/joins,
dash patterns), clipping, embedded **TrueType / Type1 / Type1C / CID-Type0** and
**Type3** fonts, the standard-14 fonts, **inline and XObject images** (Flate,
JPEG/DCT incl. CMYK, CCITT G3/G4, 1–16 bpc, `/Decode`, soft masks, stencil and
color-key masks, Indexed palettes), **Form XObjects**, **axial/radial gradients**
(`sh` and shading patterns), all 16 **blend modes**, and
DeviceGray/RGB/CMYK/ICCBased/Indexed/Lab/Separation/DeviceN color. **Encrypted**
documents (RC4, AES-128, AES-256; empty user password) decrypt transparently.
Pages honor CropBox and `/Rotate`. Invisible OCR text layers (text render
mode 3) are correctly not painted over scanned images.

## Known limitations

- **Tiling patterns** (hatches/textures) paint a neutral gray placeholder.
- **ExtGState soft masks** (vignettes, gradient-faded groups) are ignored.
- **Non-embedded CJK fonts** render no glyphs yet (embedded fonts are fine).
- **Annotations** (form fields, stamps, highlights) are not yet drawn.
- **JBIG2 / JPEG-2000** compressed images are skipped.
- **Password-protected PDFs** (non-empty user password) won't decrypt — there
  is no password prompt/API yet.
- **Dense CJK text** can differ from the CPU renderer by ~1–1.4% of pixels at threshold
  16 (anti-aliasing only — the text is correct). It passes at threshold ~24–32.

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `--backend wgpu requires building with --features gpu` | Rebuild with `--features gpu`. |
| GPU render errors with "no compatible GPU adapter found" | No usable GPU. Use `--backend cpu`, or set `ZPDF_GPU_FORCE_FALLBACK=1` if a software adapter (e.g. lavapipe/WARP) is installed. |
| Encrypted PDF renders blank | A non-empty user password is required to open it; only empty-password (owner-locked) decryption is supported. |
| Blank image areas on a scanned PDF | The images may be JBIG2 or JPEG-2000 compressed — not yet supported, see [CHANGELOG.md](CHANGELOG.md). |
| Output size differs by 1px from an old golden image | Raster dims now use `ceil` (matches pdfium); re-bless goldens. |
