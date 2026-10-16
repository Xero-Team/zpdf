# Path memory

> Issue #16: how display-list paths are stored, what that costs on line-heavy (CAD) and
> curve-dense pages, and how to reproduce the numbers. Font performance is in `fonts.md`.

## Representation

Before, `Path` was a `Vec<PathElement>`, and every element paid for the largest variant:
`CurveTo` carries three 16-byte points, so a `MoveTo`, `LineTo` or `Close` also took 56 bytes.
CAD sheets, maps and charts are almost entirely moves and lines.

`Path` now keeps points and one-byte verbs in a single allocation: points fill the buffer
from the front, verbs fill it from the back, 16 to a slot. Reading it yields the same
`PathElement` values by value. Boxing the cubic's control points (the issue's first proposal)
was rejected: it adds an allocation per curve, and glyph clip outlines and curve-dense art are
mostly curves.

| | Before | After |
|---|---:|---:|
| `size_of::<Path>()` | 24 B | 32 B |
| `size_of::<RenderCommand>()` | 136 B | 136 B (the largest variant is still `DrawGlyphRun`) |
| Move or line segment | 56 B | 17 B |
| Cubic segment | 56 B | 49 B |
| Close | 56 B | 1 B |
| Heap for a display-list `m l S` path under a non-identity CTM | 224 B (capacity 4) | 48 B (one allocation, exact size) |

The interpreter also keeps its user-space path buffer between path objects, and transforms it
into an exact-size page-space copy (`Path::map_points`) instead of pushing element by element.

## Reproducing

```bash
cargo run -p zpdf-benches --release --example path_memory -- --runs 5 \
  tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-700301-0.pdf \
  tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-692152-0.pdf \
  tests/failed/batch3/MOZILLA/MOZILLA-1244729-0.pdf \
  tests/failed/batch4/LIBRE_OFFICE/LIBRE_OFFICE-53228-1.pdf \
  tests/failed/batch6/evince/evince-1013-0.pdf \
  tests/failed/batch3/MOZILLA/MOZILLA-775768-1.zip-0.pdf
```

The example always measures two generated A1 pages: a CAD sheet in 0.1 pt units (200k
`m l S` segments, 2k 50-vertex polylines, 20k `re f` hatch rectangles) and a curve sheet (50k
four-curve circles, 10k open arcs). The six corpus pages are the most path-heavy of the
checked-in corpus. A counting global allocator reports heap peak, retained heap and
`alloc`/`realloc` calls. The `raster` column hashes the rendered pixels.

For the baseline, build the same example from the previous commit in a separate checkout with
its own target directory, changing only its `elements` helper to
`path.elements.iter().copied()`, and confirm that the two binaries' SHA-256 differ.

## Results

Linux 6.1, Intel Xeon Gold 6133 (idle), Rust 1.96.0, page 0 at 72 DPI, fastest of 5 runs.
Each cell is before → after. Heap columns vary by less than 0.2% between runs and match an
Apple M5 run to the same margin. *Retained* is the heap the interpreted page keeps (display
list plus page caches), which a viewer pays for every cached page.

| Page | Paths | Elements (lines / curves) | Retained MiB | Interpret peak MiB | Interpret allocations | Interpret ms | Render ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| Synthetic CAD sheet | 222,000 | 600,000 (358,000 / 0) | 92.1 → 46.3 | 101.1 → 55.4 | 1,242,461 → 964,466 | 423 → 421 | 2,738 → 2,660 |
| Synthetic curves | 60,000 | 320,000 (0 / 210,000) | 25.6 → 19.9 | 36.1 → 30.5 | 550,037 → 440,040 | 321 → 309 | 4,213 → 4,239 |
| GHOSTSCRIPT-700301-0 | 250,005 | 1,250,025 (750,015 / 0) | 174.8 → 87.1 | 174.9 → 87.1 | 12,006,764 → 11,506,580 | 2,150 → 2,044 | 208 → 210 |
| GHOSTSCRIPT-692152-0 | 110,849 | 576,953 (297,678 / 71,206) | 78.5 → 45.3 | 79.3 → 46.0 | 1,569,613 → 1,232,348 | 565 → 513 | 859 → 875 |
| MOZILLA-1244729-0 | 62,934 | 409,884 (255,554 / 20,345) | 316.7 → 291.6 | 317.8 → 292.7 | 8,896,372 → 8,770,814 | 2,098 → 2,148 | budget |
| LIBRE_OFFICE-53228-1 | 36,288 | 215,489 (143,455 / 0) | 32.4 → 20.3 | 49.3 → 48.9 | 1,262,576 → 1,119,123 | 476 → 457 | 508 → 509 |
| evince-1013-0 | 29,507 | 143,521 (41,173 / 33,866) | 19.5 → 12.8 | 22.6 → 15.9 | 261,358 → 211,980 | 178 → 173 | budget |
| MOZILLA-775768-1.zip-0 | 893 | 27,921 (4,873 / 21,192) | 19.7 → 18.4 | 37.1 → 37.1 | 265,743 → 262,377 | 629 → 628 | 1,343 → 1,330 |

- Line-heavy pages keep about half the heap. Pages whose retained heap is mostly images or
  fonts (MOZILLA-1244729) save the path share only.
- Interpretation makes fewer allocations and is not slower; time differences are within run
  noise.
- Rendering is unchanged: the same peak heap and allocation count, and identical raster
  hashes. The exceptions are the pages marked *budget*, which hit the 8 s render budget on
  this CPU; their output depends on where the budget stops, so their hashes vary from run to
  run in both builds.

## Compatibility

`Path::elements` is no longer public; this is a breaking change for code that builds or reads
paths directly. The migration (`path.iter()`, `for element in &path`, `push`, `extend`,
`collect`, `points`) is listed in `docs/CHANGELOG.md`. `PathElement` is unchanged.
