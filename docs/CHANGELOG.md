# Changelog

## 0.2.0 — wgpu GPU rendering backend

This release adds a complete, GPU-accelerated rendering backend (`zpdf-render-wgpu`)
that renders the same `DisplayList` as the CPU renderer and matches it pixel-for-pixel
within a <1% tolerance. It also fixes two rendering bugs surfaced while validating
the backend against real-world documents.

### Highlights

- **New `wgpu` GPU backend** implementing the full `RenderBackend` contract — fills,
  strokes, clips, text (outline + Type3), images, and transparency groups.
- **`--backend wgpu`** flag on the `render` CLI command (behind the `gpu` feature).
- **Interactive viewer** example (`zpdf-render-wgpu`, winit) for pan/zoom/page-flip.
- **CI acceptance harness** that renders a synthetic corpus through both backends and
  asserts GPU↔CPU parity at <1% differing pixels.
- **Two correctness fixes** (see below) affecting text layout and viewer display.

### GPU backend (`zpdf-render-wgpu`)

Built milestone by milestone, each verified against the tiny-skia CPU renderer (the
correctness oracle) with the existing `zpdf compare` tool:

| Area | Detail |
| --- | --- |
| Headless context | `Instance`/`Adapter`/`Device`/`Queue` via `pollster`; MSAA-4x + `Stencil8` negotiated; texture-dim limit raised to the adapter max. |
| Render target | Offscreen MSAA color + resolve + `Stencil8`; `copy_texture_to_buffer` → map → padding-stripped RGBA8 readback (256-byte row alignment). |
| Fills & strokes | `lyon` tessellation in device-pixel space; non-zero / even-odd fill rules; caps/joins/miter; dashes intentionally ignored (matches the CPU oracle). |
| Clipping | Stencil-based; nested clips via `IncrementClamp`; `PopClip` rebuilds the stencil. |
| Text | **Vector-fill baseline** — glyph outlines tessellated at the CPU's exact coordinates (correct by construction); Type3 glyphs interpreted as sub-display-lists. |
| Images | Per-image `Rgba8Unorm` texture + bind group; the `render_image` affine baked into quad vertices; nearest sampling; clip-tested. |
| Blend groups | Offscreen layer stack with per-group compositing; all 16 PDF blend modes via the W3C premultiplied formula. |
| Coordinate system | Single page→NDC transform reproducing the CPU's `(x·scale, (page_height−y)·scale)` with the `ctm_flips_y` heuristic and an f32→f64→f32 precision rule. |

**Validation.** All synthetic corpus cases pass <1%. The three real-world reference
PDFs render at 0.35–0.58% (page 1). A 62-page CJK document renders 52/62 pages at
<1%; the remaining dense-CJK pages sit at 1.0–1.4% — see *Known limitations*.

### CLI

- `render … --backend [cpu|wgpu]` selects the renderer (default `cpu`). `wgpu` requires
  building the CLI with `--features gpu`.
- Both backends now save through one shared PNG path, so output is identical apart from
  the renderer.

### Fixes

- **Indirect `/Widths` not resolved (text over-spacing).** Font-width parsing did not
  resolve `/Widths` and `/W` when they were *indirect references* (the form pdfTeX/LaTeX
  emits). Every glyph fell back to a 1-em default width, so text rendered uniformly
  over-spaced ("H a u s a u f g a b e"). The width parsers now resolve a reference before
  reading the array. *Affects CPU rendering, GPU rendering, and text extraction* (glyph
  advances also position extracted text spans).
- **Viewer washed out / too bright (sRGB double-encode).** The example viewer configured
  its surface with the default (sRGB) format, so blitting the already-gamma-encoded page
  bytes re-applied gamma on store and brightened the image. The viewer now selects a
  non-sRGB surface format for a 1:1 blit. *Viewer-only — the core renderer's pixels were
  always correct.*

### Known limitations

- **Dense CJK text AA.** tiny-skia uses analytic anti-aliasing; the GPU uses 4× MSAA.
  On glyph-dense CJK pages the per-edge-pixel difference can exceed the threshold-16 gate
  (~1.0–1.4% of pixels), though the text renders correctly. 8× MSAA was evaluated and made
  the match *worse* (a blend-space mismatch), so 4× is used. These pages pass at
  threshold ~24–32.
- **Transparency groups are dormant.** The content interpreter does not yet emit
  `PushBlendGroup`/`PopBlendGroup` (no `/BM` or transparency-group handling), so blend
  groups don't arise from real PDFs on *either* backend. The GPU implementation is ready
  for when interpreter support lands; it's validated programmatically.
- **Not implemented (intentionally deferred):** draw-call batching (immediate mode is
  correct and fast enough), an R8 glyph atlas (the vector baseline is a better AA match),
  GPU timing telemetry, and soft masks.

### Dependencies added

`wgpu 29`, `lyon 1`, `pollster` (GPU backend); `winit 0.30` (viewer example only — a
dev-dependency, never linked into the library or CLI).
