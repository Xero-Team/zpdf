# zpdf User Guide

`zpdf` is a pure-Rust PDF parser and renderer. This guide covers the `zpdf`
command-line tool: rendering pages to PNG (on CPU or GPU), converting to TXT,
Markdown, or HTML, extracting text,
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
| `info` | Print version, page count, per-page size/rotation, metadata, and outline summary. |
| `render` | Render a page to a PNG (CPU or GPU). |
| `text` | Extract text from a page (or all pages). |
| `convert` | Convert selected pages to TXT, Markdown, or HTML with optional PNG assets. |
| `tables` | Detect tables on a page (or all pages) and print them as TSV/CSV. |
| `forms` | List interactive-form (AcroForm) fields, types, and values. |
| `outline` | Print the document outline (bookmarks) as an indented tree with resolved targets. |
| `attachments` | List (and optionally extract) embedded & associated files. |
| `compare` | Pixel-diff two PNGs and report difference metrics. |
| `dump` | Print a resolved PDF object. |
| `debug-stream` | Print a decoded stream object's bytes. |

> **Encrypted PDFs.** Documents protected with a non-empty password open with
> `--password <pw>` (accepted by `info`, `dump`, `render`, `text`, `convert`, `tables`,
> `forms`, `outline`, and `attachments`).
> The password may be the user or owner password; a wrong one reports an error.
> Documents encrypted with an empty password open without the flag.

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
Metadata:
  Title: Annual Report
  Author: Jane Doe
  Producer: LibreOffice 7.6
  Created: D:20240101120000Z
Outline: 4 top-level bookmark(s), 27 total
```

The `Metadata` block lists the document information dictionary (`/Info`) fields
that are present (title, author, subject, keywords, creator, producer, creation
and modification dates). `Output Intents`, `Embedded files`, and
`Associated files` sections follow when the document carries them.

### `outline` — list bookmarks

```bash
cargo run -p zpdf-cli -- outline document.pdf
```

Prints the document outline (`/Outlines` bookmarks) as an indented tree, each
line ending in its resolved target — `-> p.<N>` for an in-document page (1-based)
or `-> uri:<url>` for a hyperlink:

```
Cover  -> p.1
Introduction  -> p.2
  Background  -> p.3
  Goals  -> p.5
Appendix  -> p.40
References  -> uri:https://example.org/refs
```

Each item's `/Dest` (explicit array or named destination, resolved through both
the `/Names /Dests` name tree and the legacy `/Root /Dests` dictionary) or its
go-to / URI action (`/A`) is resolved to a page or link. A document with no
outline prints `No document outline (bookmarks).`

### `tables` — detect tables

```bash
cargo run -p zpdf-cli -- tables document.pdf -p 1        # one page
cargo run -p zpdf-cli -- tables document.pdf --all --csv # every page, CSV
```

Detects tabular layouts from text-span geometry (alignment / whitespace-grid
heuristics) and prints each as TSV (or CSV with `--csv`).

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

### `convert` — write TXT, Markdown, or HTML

```bash
# Whole-document plain text. Images and all other graphics are not decoded.
cargo run -p zpdf-cli -- convert document.pdf -o document.txt --mode text

# Rich Markdown plus document_assets/page-NNNN-image-NNN.png files.
cargo run -p zpdf-cli -- convert document.pdf -o document.md --mode rich

# Rich, styled HTML using the same extracted text, metadata, and PNG assets.
cargo run -p zpdf-cli -- convert document.pdf -o document.html --mode rich

# Selected pages in Tagged-PDF logical reading order when available.
cargo run -p zpdf-cli -- convert document.pdf --mode rich --pages 1,3-5 \
  --struct --images-dir assets -o excerpt.md
```

`--mode text` is the default. It constructs the content interpreter without an
image cache, so image streams, shadings, and other graphics are silently skipped
and cannot interfere with text extraction. TXT contains only extracted page text,
with pages separated by blank lines. `--format txt|md|html` overrides format
inference from `-o`; without `-o`, output defaults to the input name with `.txt`,
`.md`, or `.html`.

`--mode rich` requires Markdown or HTML. It adds `/Info` and XMP metadata, page
size, rotation, printed page labels, and every successfully decoded raster image.
Images are written as PNG files under `<output-stem>_assets` by default. An
unsupported, malformed, over-budget, or unwritable image is omitted while
conversion continues with the page text. Repeated draws of the same image export
one asset and retain a placement count. HTML output is a complete UTF-8 document
with responsive light/dark CSS, semantic page sections, escaped metadata/text,
and preformatted text that preserves extracted line breaks and spacing.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--mode text\|rich` | `text` | Text only, or metadata and images where supported. |
| `--format txt\|md\|html` | output extension | Output serialization; rich mode requires `md` or `html`. |
| `-o, --output <file>` | input stem | Destination `.txt`, `.md`, or `.html` file. |
| `-p, --page <n>` | all pages | Convert one 1-based page. |
| `--pages <list>` | all pages | Convert a list such as `1,3-5`; duplicates are removed. |
| `--all` | all pages | Explicitly convert the complete document. |
| `--struct` | off | Prefer Tagged-PDF logical order, falling back to geometric order. |
| `--images-dir <dir>` | `<output-stem>_assets` | Rich-mode PNG destination, relative to the Markdown/HTML file. |

### `attachments` — list & extract embedded files

```bash
cargo run -p zpdf-cli -- attachments document.pdf                       # list
cargo run -p zpdf-cli -- attachments document.pdf --extract all --out-dir files
cargo run -p zpdf-cli -- attachments invoice.pdf --extract factur-x.xml # one file
```

Lists files **embedded inside** the PDF — both classic attachments (the catalog
`/Names /EmbeddedFiles` name tree) and PDF 2.0 *associated files* (`/AF`, which
carry an `/AFRelationship` such as `Data` or `Source`; this is how ZUGFeRD /
Factur-X e-invoices embed their source XML). Each line shows the name and any
relationship, MIME subtype, and declared size.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--extract <index\|name\|all>` | — | Extract one file by listing index or name, or `all`. |
| `--out-dir <dir>` | `.` | Directory to write extracted files into (created if needed). |

Extraction is safe against hostile file names: a `/UF` like `../../etc/passwd`
is reduced to its basename; path separators, Windows-reserved characters /
device names, and trailing dots/spaces are neutralized; existing files are never
overwritten and same-name collisions get a ` (n)` suffix — an attachment can
neither be written outside `--out-dir` nor clobber a file already in it. Use the
listing index (e.g. `--extract 0`) to pull out an unnamed or duplicate-named file.

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
DeviceGray/RGB/CMYK/ICCBased/Indexed/Lab/Separation/DeviceN/**NChannel** color
(the `None` colorant correctly produces no marks; `All` knocks out). **Encrypted**
documents (RC4, AES-128, AES-256; empty user password) decrypt transparently.
**Annotations** render from their appearance streams; markup and geometric
annotations (highlights, underlines, strike-outs, squiggles, squares, circles,
lines, polygons, ink, free text, sticky-note and stamp icons, caret insertion
marks, redaction-region marks) and interactive **form fields** get a synthesized
appearance when the producer left none. Pages honor CropBox and `/Rotate`.
Invisible OCR text layers (text render mode 3) are correctly not painted over
scanned images.

## Known limitations

- **Tiling patterns** (hatches/textures) paint a neutral gray placeholder.
- **ExtGState soft masks** (vignettes, gradient-faded groups) are ignored.
- **Non-embedded CJK fonts** render no glyphs yet (embedded fonts are fine).
- **Annotation appearances** are synthesized for most markup/geometric subtypes
  when absent, but some fidelity trade-offs remain: a `Redact` mark shows the
  *marked* region (content is never actually removed, and `/OverlayText` is not
  drawn); a `Caret` does not distinguish the `/Sy` paragraph variant; rotated /
  skewed text-markup uses an axis-aligned approximation. The PDF 2.0 `Projection`
  subtype has no synthesized default appearance.
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
