# veraPDF Corpus Compatibility Report

Harness: `tests/corpus_run.sh` — renders page 1 of every PDF under
`tests/veraPDF-corpus/` at 72 DPI via `zpdf render` (release build, 20 s
timeout per file) and writes a TSV of `status path message`.

The corpus (see its README) is 2,907 atomic test files for PDF/A, PDF/UA,
ISO 32000-1/2, plus the Isartor and TWG suites. Most "fail" files violate a
*conformance* clause while remaining structurally valid PDFs, so a robust
renderer is expected to handle nearly all of them.

## Results

| Run | OK | FAIL | TIMEOUT | Pass rate |
| --- | --- | --- | --- | --- |
| Baseline (commit `afd164e`) | 2,902 | 3 | 2 | 99.83 % |
| After fixes | **2,907** | 0 | 0 | **100 %** |

Result files (written locally by the harness, not checked in):
`tests/corpus_results.tsv` (baseline), `tests/corpus_results_after.tsv`
(after fixes).

## Marked files (baseline failures) and root causes

### 1. Malformed header version — `Error: not a PDF file`

- `PDF_A-1b/6.1 File structure/6.1.2 File header/veraPDF test suite 6-1-2-t01-fail-b.pdf`

The header is `%PDF-a.4`: an intentionally invalid version digit (the file is
otherwise a perfectly normal PDF). `parse_header` rejected the whole file.

**Fix** (`zpdf-parser/src/header.rs`): when the `%PDF-` magic is present but
the version is malformed/truncated, warn and assume PDF 1.7 — matching the
leniency of other robust readers. `NotAPdf` is now returned only when the
magic is missing entirely.

### 2. String over 64 KiB — `Error: string length exceeded (max 65536 bytes)`

- `PDF_A-1b/6.1 File structure/6.1.12 Implementation limits/veraPDF test suite 6-1-12-t03-fail-d.pdf`
- `TWG test files/TWG test suite A009-pdfa1-fail-b.pdf`

Both contain a 65,542-byte literal string (`/XXCustom`, deliberately a few
bytes over the PDF/A-1 implementation limit). ISO 32000 itself imposes **no**
string length limit — 64 KiB is only the PDF/A-1 / legacy-Acrobat bound — so
rendering must tolerate it.

**Fix** (`zpdf-core/src/limits.rs`): default `ParseLimits::max_string_length`
raised 64 KiB → 16 MiB. It remains a configurable allocation guard.

### 3. Tiling pattern × soft mask blow-up — timeout (>100 s for a 4 KB file)

- `PDF_A-2b/6.2 Graphics/6.2.10 Transparency/veraPDF test suite 6-2-10-t06-fail-d.pdf`
- `PDF_A-4/6.2 Graphics/6.2.9 Transparency/veraPDF test suite 6-2-9-t06-fail-d.pdf`

A 500×500 pt page filled with a tiling pattern (BBox 80×80, XStep/YStep 15 →
1,681 tiles) whose cell applies an ExtGState `/SMask` over a shading. Each
tile re-interpreted the mask group and re-rasterized a 768×768 gradient
(~45 ms), then the CPU backend re-rasterized the full-page mask plane per
painted command (~15 ms) — ~100 s total.

**Fix** (two layers, exact for the tiling case — the 196-tile reduction of
this file renders pixel-identical to the pre-fix output):

- *Interpreter* (`zpdf-content/src/interpreter.rs`): a tile-loop-scoped
  soft-mask cache. Tile CTMs differ only by translation, so a mask built once
  (rebased to the canonical middle tile, whose cell the page-rect raster
  window fully covers) is reused on every tile with a page-space
  `SoftMask::offset`. The cache key is (per-tile operator index, ExtGState
  id, CTM linear part), making collisions between distinct `gs` sites
  impossible.
- *CPU backend* (`zpdf-render-cpu/src/lib.rs`): rasterized mask planes are
  cached per mask identity (Arc pointer + parameters); offset uses are
  derived by a device-pixel shift-blit with the mask's unpainted value
  filling vacated strips.

Both files now render in ~4.5 s (remaining cost is the per-command blend
group compositing, a possible future optimization — bounding blend groups to
the painted command's bbox instead of the full page would cut it further).

## Reproducing

```bash
cargo build --release -p zpdf-cli
tests/corpus_run.sh            # writes tests/corpus_results.tsv
```
