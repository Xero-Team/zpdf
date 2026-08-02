## zpdf 0.12.0 — SVG export + WebAssembly

Two new subsystems leveraging zpdf's pure-Rust architecture:

### SVG Export (`zpdf-svg-export`)

Vector-faithful page → SVG conversion. Converts the flat `DisplayList` (the same stream the CPU/GPU backends consume) to standalone SVG:

- Paths, glyphs, and strokes stay vectors
- Images embed as base64 PNG
- Clips/soft masks/blend modes map to SVG equivalents (clips as attribute chains, never wrapped in `<g clip-path>` which would isolate blending)
- Coordinates in PDF points with Y-flip baked in; `viewBox` = page box at 72 dpi

Verified against the CPU backend via resvg rasterization (zero systematic drift), hardened against the 618-PDF adversarial corpus (zero panics/timeouts).

```bash
cargo run -p zpdf-cli -- export-svg input.pdf -p 1-5 -o page-%d.svg
```

### WebAssembly (`zpdf-wasm`)

Compiles the full read pipeline to `wasm32-unknown-unknown` — parse, render (CPU backend), text extraction, SVG export — all in the browser or Node. Pure Rust with zero C dependencies is what makes this build feasible where pdfium/MuPDF would struggle.

```bash
bash crates/zpdf-wasm/build-web.sh
python -m http.server -d crates/zpdf-wasm/www 8080
```

The `www/` demo is a fully client-side viewer: drop a PDF, it parses and renders in wasm. Nothing uploads. Arrow keys flip pages; text extraction and SVG export work on the current page. ~2.8 MiB wasm (~900 KiB gzipped).

### API

```js
import init, { Pdf } from "./pkg/zpdf_wasm.js";
await init();

const pdf = Pdf.open(bytes);  // or: Pdf.open_with_password(bytes, "pw")
const bitmap = pdf.render_page(0, dpi / 72);
const text = pdf.page_text(0);
const svg = pdf.page_svg(0);
```

All 18 workspace crates now compile for native + wasm32. Wall-clock budgets are disabled on bare wasm32 (no `Instant::now`); deterministic `ParseLimits` still bound adversarial inputs.

---

**Changes:**
- **Added** `zpdf-svg-export` crate + `zpdf-cli export-svg` subcommand
- **Added** `zpdf-wasm` crate with `wasm-bindgen` API + browser demo (`www/`)
- **Fixed** 32-bit overflow guards in `zpdf-writer` xref/ByteRange emitters (wasm32 compat)
- **Docs** CLAUDE.md updated for the new crates (workspace now 18 crates)
