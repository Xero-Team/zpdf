# zpdf-svg-export

Vector-faithful SVG export from zpdf display lists.

Converts the flat `DisplayList` command stream — the same one the CPU and GPU render backends consume — into standalone SVG documents. Semantics mirror the CPU backend command-for-command:

- Solid fills/strokes → SVG `<path>` with `fill`/`stroke` attributes
- Text → glyph-outline `<path>` elements (Type3 glyphs via their interpreted vector content)
- Images → embedded base64 PNG (`<image href="data:image/png;base64,...">`)
- Clips → `<clipPath>` defs, intersected via attribute chains (never wrapped in `<g clip-path>`, which would isolate blending)
- Soft masks → luminosity `<mask>` with nested display-list recursion
- Blend modes → `mix-blend-mode` on painted elements

Coordinates are emitted in PDF points with the page's Y-flip baked in (`svg = (x - x0, y1 - y)`), so the `viewBox` equals the page box at 72 dpi and no root `transform` is needed.

## Usage

```rust
use zpdf_svg_export::{display_list_to_svg, SvgOptions};

let svg = display_list_to_svg(&display_list, fonts, images, &SvgOptions::default());
std::fs::write("page.svg", svg)?;
```

Via CLI:

```bash
cargo run -p zpdf-cli -- export-svg input.pdf -p 1-5 -o page-%d.svg
```

## Options

```rust
SvgOptions {
    background: Some(Color::white()),  // None for transparent canvas
    budget: Some(Duration::from_secs(8)),  // wall-clock anti-hang (mirrors CPU backend)
}
```

Adversarial display lists (huge Type3 fan-outs, thousands of pattern-cell images) stop emitting paint once the budget expires; the document stays well-formed.

## Approximations

Logged once per export via `tracing`:

- Overprint composites paint normally (PDF overprint affects device colorants; SVG has no equivalent)
- Knockout groups composite as normal groups (transparency group isolation semantics differ)
- Soft-mask `/TR` transfer functions are ignored (not expressible in SVG luminosity masks)

## Verification

The test suite (`tests/fidelity.rs`) compares SVG output against the CPU backend's raster oracle: export → resvg rasterize → pixel diff. Zero systematic drift across the synthetic corpus and all test PDFs.

Robustness: the same 618-PDF adversarial harness (`tests/failed/`) that validates the parsers and renderers runs against the SVG exporter — zero panics, zero timeouts (the 8s budget bounds wall-clock time).
