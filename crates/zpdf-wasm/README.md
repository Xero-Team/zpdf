# zpdf-wasm

WebAssembly bindings for zpdf, exposing the read pipeline (parse, render, text extraction, SVG export) to JavaScript in the browser or Node.

Pure Rust with zero C dependencies — no pdfium, no MuPDF, no `poppler` — makes this build feasible on `wasm32-unknown-unknown` where native-library ports would struggle. The CPU backend's tiny-skia rasterizer and the SVG exporter's direct DisplayList translation run entirely in your browser; no uploads, no server round-trips.

## Build

Requires `wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the version matching `Cargo.lock`:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.122  # match Cargo.lock
bash crates/zpdf-wasm/build-web.sh
```

This compiles for wasm32 and generates ES-module bindings into `www/pkg/`.

## Demo

The `www/` directory is a fully client-side PDF viewer: drop a file, it parses and renders in wasm. Nothing leaves your machine.

```bash
python -m http.server -d crates/zpdf-wasm/www 8080
# or any static server: npx serve www, etc.
```

Open `http://localhost:8080` and drop a PDF. Arrow keys flip pages; Extract text and Download SVG work on the current page.

## API

```js
import init, { Pdf } from "./pkg/zpdf_wasm.js";
await init();  // load the wasm

const pdf = Pdf.open(bytes);  // Uint8Array
// or: Pdf.open_with_password(bytes, "secret")

pdf.page_count  // number
pdf.title       // string | undefined
pdf.page_size(0)  // [width_pt, height_pt]

const bitmap = pdf.render_page(0, scale);  // scale = dpi / 72
// bitmap.width, bitmap.height (pixels), bitmap.rgba() → Uint8Array
const imgData = new ImageData(new Uint8ClampedArray(bitmap.rgba().buffer), bitmap.width, bitmap.height);
canvas.getContext("2d").putImageData(imgData, 0, 0);

const text = pdf.page_text(0);  // reading-order plain text
const svg = pdf.page_svg(0);    // standalone vector SVG
```

## Editing

`PdfEditor` wraps the incremental writer: open a document, queue any number of
edits, then `save()` to serialize them as **one incremental update** (the
original bytes plus the new revision — the smallest possible output). Page
indices are 0-based.

```js
import init, { PdfEditor, split_pages, optimize, OptimizeOptions } from "./pkg/zpdf_wasm.js";
await init();

const ed = PdfEditor.open(bytes);
ed.rotate_page(0, 90);                 // 0-based page index
ed.stamp_text(0, "DRAFT", 100, 100, "Helvetica", 24, 1, 0, 0);  // r,g,b in [0,1]
ed.fill_fields(["name", "qty"], ["Alice", "3"]);                  // parallel arrays
ed.set_title("Edited in the browser");
const out = ed.save();   // Uint8Array — one incremental update
ed.free();

// One-shot helpers (byte-in / byte-out — a full rewrite or extract):
const onePage = split_pages(bytes, [0]);          // extract page 0 → new PDF
const small = optimize(bytes, (() => { const o = new OptimizeOptions(); o.compress = true; o.max_image_dim = 1500; return o; })());
const fast = optimize(bytes, (() => { const o = new OptimizeOptions(); o.linearize = true; return o; })());  // fast web view
```

`PdfEditor` methods: `rotate_page`, `delete_pages`, `reorder_pages`,
`set_title/author/subject/keywords/creator/producer` (`Some` sets, `null`
deletes, not calling leaves unchanged), `stamp_text`, `stamp_image_rgba`,
`add_highlight/underline/strikeout/squiggly`, `add_note`, `add_freetext`,
`add_square`, `add_circle`, `add_line`, `redact` (true content removal),
`fill_fields`, `merge`. Encryption and PDF/A conversion are not exposed to
wasm in this round — use the native `zpdf` facade for those.

All methods are synchronous (the wasm module doesn't yield back to JS mid-page). Heavy PDFs might block the main thread — wrap in a Web Worker for production.

## Limitations

- Wall-clock anti-hang budgets are disabled (bare wasm32 has no `Instant::now`); the deterministic budgets (operator/command counts, pixel caps) in `ParseLimits` still bound adversarial inputs.
- No GPU backend (WebGPU is API-compatible but the buffer/layout code assumes native-sized pointers; a `wasm32` build of `zpdf-render-wgpu` is planned but not wired yet).
- Signature verification works (p256/p384 via RustCrypto), but CRL/OCSP revocation checks aren't implemented.

## Size

The release `.wasm` is ~2.8 MiB before gzip (includes font subsetting, color management, all of zpdf-parser/document/content/render-cpu/svg-export, plus signature crypto). Typical gzip transfer is ~900 KiB. For production, disable unused crate features and run `wasm-opt` for another 10–15% win.
