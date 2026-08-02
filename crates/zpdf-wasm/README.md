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

All methods are synchronous (the wasm module doesn't yield back to JS mid-page). Heavy PDFs might block the main thread — wrap in a Web Worker for production.

## Limitations

- Wall-clock anti-hang budgets are disabled (bare wasm32 has no `Instant::now`); the deterministic budgets (operator/command counts, pixel caps) in `ParseLimits` still bound adversarial inputs.
- No GPU backend (WebGPU is API-compatible but the buffer/layout code assumes native-sized pointers; a `wasm32` build of `zpdf-render-wgpu` is planned but not wired yet).
- Signature verification works (p256/p384 via RustCrypto), but CRL/OCSP revocation checks aren't implemented.

## Size

The release `.wasm` is ~2.8 MiB before gzip (includes font subsetting, color management, all of zpdf-parser/document/content/render-cpu/svg-export, plus signature crypto). Typical gzip transfer is ~900 KiB. For production, disable unused crate features and run `wasm-opt` for another 10–15% win.
