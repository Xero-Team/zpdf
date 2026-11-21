# zpdf vs hayro and minipdf: Gap Analysis

## Document control

| Field | Value |
|---|---|
| Date | 2026-11-19 |
| zpdf base | Measured on `8779a86` (0.18.0); fixes rebased onto `d4fe5a8` (0.20.0) |
| Compared | hayro 0.9.0 family (hayro, -syntax, -interpret, -font, -cmap, -jbig2, -jpeg2000, -ccitt, -svg, -write, -postscript); minipdf 0.7.0 |
| Status | Nine gaps fixed in the working tree (see CHANGELOG "Unreleased"); the rest ranked below |

### Rebase and audit follow-up (2026-11-21)

PR #154 is rebased onto `cc26be6` (the merged glyph-selection work in #152).
The corpus measurements below are historical results from the original PR;
they have not been remeasured for this audit. The remaining-gap inventory also
describes the original comparison, rather than a fresh inventory of main.

The follow-up corrects the reviewed resource and fidelity problems:

- Encrypted recovery counts direct plus unique compressed objects before
  collecting new entries. Open-time exhaustion returns an error; lazy repair
  memoizes the failed scan. Duplicate compressed members use the later container
  offset, with direct objects retaining precedence.
- If resolving a missing `/Encrypt` reference built the lazy scan before
  authentication, upgrade that cached scan once the decryptor exists. A
  synthetic stale-xref test previously returned null for the encrypted page
  tree and now resolves it correctly.
- JBIG2 generic/refinement regions share a work ceiling of twice the configured
  per-image pixel cap (itself bounded by the internal bitmap cap), charged as
  area plus row count before allocation. Globals, page-info resets and failed
  regions do not renew that allowance. Exceeding it fails the decode; legitimate
  sparse regions retain their area-based MQ allowance.
- Mask upsampling computes coordinates on demand, with constant auxiliary
  mapping storage. It checks source arithmetic and incomplete alpha planes
  before allocating the enlarged image. Existing pixel-centre interpolation
  and high-resolution mask behaviour remain covered.
- JPX Indexed decoding reads original component samples, preserving 2-, 4-
  and 8-bit indices even when a JP2 palette is present. Synthetic fixtures and
  their generator are included.
- JPX also checks decoded component bytes against `max_decoded_stream_bytes`
  before allocating decoder planes. This bounds the memory amplification of
  the newly accepted multi-colorant streams; pixel count alone was insufficient.
  A two-channel regression fails one byte below the required count and succeeds
  exactly at it. The limit applies to all JPX component counts.
- Flate uses the inflater's full 32 KiB dictionary-sized output buffer, retaining
  bytes produced immediately before a corrupt block fails. Output reservations
  remain mandatory; corruption does not bypass the decode budget.
- Recovering embedded minus signs retains the numeric value and records known
  malformed content for strict callers.

Regression tests reproduce the original aggregate-object, revision-selection,
Flate-prefix, tokenizer-diagnostic and cumulative-JBIG2 failures. The follow-up
also checks exact-limit success, memoized failures, byte-exact JPX palette
selection, skinny masks and malformed image geometry. Public APIs and the
pure-Rust runtime dependency policy are unchanged.

Final validation on macOS arm64 with Rust 1.100.0:

| Check | Result |
|---|---|
| `cargo +1.100.0 fmt --all --check` and `git diff --check` | Passed |
| `cargo +1.100.0 clippy --workspace --all-targets --offline -- -D warnings` | Passed; existing Cargo manifest and third-party future-compatibility notices remain |
| `cargo +1.100.0 test --workspace --offline --no-fail-fast` | 1,816 passed, 0 failed, 2 ignored |
| `build-web.sh` with Rust 1.100.0 and wasm-bindgen 0.2.129 | Release WASM rebuilt; all 9 package/fingerprint checks passed |
| Font fixtures and Adobe CMap regeneration checks | Passed |
| Font/CMap Python helper tests and browser font-pack tests | 14 Python tests and 5 Node tests passed |
| New indexed JP2 fixture regeneration | All three files reproduced byte-for-byte |

The final diff was reviewed again after the fixes; no additional blocking
findings remain in the reviewed changes. Inverse regression runs confirmed
that removing the palette and malformed-mask fixes makes their tests fail.
This is an agent audit, not a human approval or an exhaustive security proof.
The 506-file comparison and Linux/Windows-specific checks were not rerun locally.
Keeping full mask resolution still costs more than shrinking it. Valid but
unusually large JPX images or repeated JBIG2 regions can now hit the configured
resource limits; callers can raise their limits when appropriate.

## 1. What the two projects are

**hayro** (LaurenzV, MIT/Apache-2.0, ~74k lines across 11 crates) is a pure-Rust
PDF rasterizer on vello_cpu + kurbo, with skrifa/read-fonts for all font
outlines and hinting. Its regression suite is about 1,000 PDFs scraped from
pdf.js and PDFBox. It has no writer, signatures, text extraction, or GPU path.
Its own docs list knockout groups and color-key masking as missing.

**minipdf** (mini-software, Apache-2.0, ~14k lines) is not a PDF reader. It is an
Office (DOCX/XLSX/PPTX) → PDF 1.4 converter. It overlaps with zpdf only in the
authoring path (`zpdf-writer`).

Licences are compatible with zpdf's MIT. Ported logic should credit the source
file in a comment.

## 2. Method

1. **Differential render.** A scratch harness rendered page 1 of all 506 local
   PDFs (`tests/corpus` plus the PDFBox, Ghostscript, pdf.js, poppler and other
   sets in `tests/failed`) at 72 dpi with both engines. Each engine ran in a
   process with a 30 s timeout and a 3 GiB address-space cap. The harness
   recorded status, wall time and a pixel diff: the fraction of pixels whose
   largest channel delta exceeds 48 after compositing on white. Every page
   diverging by more than 5% was compared visually.
2. **Code review.** Four parallel reviews compared fonts/text, image codecs,
   syntax/filters, and interpretation/rendering, citing file:line on both
   sides. Five parser findings and two image findings were reproduced with
   probes.

Caveats:
- hayro-syntax enables flate2's `zlib-rs`. In the harness, Cargo feature
  unification applied it to zpdf too, so both engines inflated with the same
  code.
- Timings come from three parallel jobs on a 16-thread, 15 GiB laptop. Read
  them as ratios, not benchmarks.

## 3. Results

| | Before | After this change |
|---|---|---|
| Files zpdf renders (hayro: 191, plus 8 hayro panics) | 293 | 301 |
| Pages zpdf reports complete | 165 | 189 |
| Pages diverging >5% from hayro | 42 | 33 |
| GHOSTSCRIPT-700301-0 (CLI) | 1.74 s | 0.01 s |

Re-measured after rebasing onto 0.20.0 (`d4fe5a8`, same corpus and
harness; hayro renders reused):

| | origin/main 0.20.0 | This change |
|---|---|---|
| Files zpdf renders | 293 | 301 (none newly failing) |
| Pages zpdf reports complete | 165 | 189 |
| Pages diverging >5% from hayro | 42 | 33 |
| Total zpdf time, 293 files both render | 104.0 s | ≈104.5 s |

The total for this change substitutes the final PDFBOX-3948-0 timing
(≈505 ms, after the single-pass upsampler) for the 1,437 ms measured
before it. The rest of the difference comes from pages that now draw
content.

Notable per-file times (origin/main → this change):
- GHOSTSCRIPT-700301-0: 1,877 → 45 ms.
- PDFBOX-1508-0 zip-1/zip-4: ≈200 → ≈35 ms.
- pdf.js-8644-0: 271 → 133 ms.
- The 695040 family: ≈9 → ≈90 ms, now that its pages render instead of
  coming out blank.
- PDFBOX-3948-0: ≈300 → ≈505 ms, the cost of keeping mask resolution
  (see §4).

Two pages flipped from "complete" to "incomplete": 695040-0 zip-55 and
zip-65. Both went from blank or near-blank to matching hayro. They now
report the one missing content stream (zip-55) and two malformed
operators (zip-65) honestly.

- Most of the remaining divergences are hayro rendering a blank page. In
  every regressed-looking case (cairo-101527-0, cairo-76-0, PDFBOX-2720-5,
  PDFBOX-3361-8, GHOSTSCRIPT-699723-0) zpdf now draws content and hayro does
  not.
- zpdf never panicked. hayro panicked on 8 files.
- On the 178 files both engines rendered before the change, zpdf's median time
  ratio to hayro was 1.09×. The 10th percentile was 0.34×, the 90th 5.47×. The
  tail was dominated by the outliers listed in §5.

**Where zpdf is already ahead of hayro:**
- Encryption: R2–R6, owner passwords, `/Crypt` filters.
- Knockout and isolated groups, overprint, OCG `/VE` and `/OCMD /P`.
- Soft-mask `/BC` and `/TR`.
- JPX `SMaskInData 2` and `/Matte`.
- JBIG2 resource budgets.
- Decompression-bomb limits.
- Vertical writing.
- More predefined CMaps.
- Everything outside rasterization: writer, signatures, extraction, exports,
  GPU.

## 4. Fixed in this change

| Gap | hayro reference | zpdf change | Files fixed in the corpus |
|---|---|---|---|
| Recovery ignored encrypted object streams | `hayro-syntax/src/xref.rs` two-pass `fallback_xref_map_inner` | `zpdf-parser/src/lib.rs`: `encrypted_objstm_members`, `build_repair_table`, `decode_objstm` retries at the repair-scan offset | GHOSTSCRIPT-695040-0 family (12 files, previously blank or unopenable), PDFBOX-4777-0/1 |
| `/lenIV -1` charstrings decrypted anyway | read-fonts Type 1 | `zpdf-font/src/type1.rs`: `decrypt_charstring` | pdf.js-8644-0 (glyphs drawn as a fan of lines) |
| JPX palette applied before PDF `/Indexed`; 2-colorant DeviceN rejected | `hayro-syntax/src/filter/jpx.rs` (`resolve_palette_indices: false`) | `zpdf-image/src/lib.rs` JPX path | GHOSTSCRIPT-691880-0, poppler-32746-0/2 (diff 0.97 → 0) |
| Bad Adler-32 lost the stream or its last 16 KiB | `hayro-syntax/src/filter/lzw_flate.rs` | `zpdf-parser/src/filters.rs`: miniz_oxide with `ZLibIgnoreChecksum`, keeps the bytes of the failing chunk | cairo-101527-0, cairo-76-0, PDFBOX-2720-5, PDFBOX-3361-8, GHOSTSCRIPT-699723-0 now draw content |
| Truncated RunLength discarded everything | `filter/run_length.rs` (PDFBOX-3990) | `run_length_truncated` | — |
| Sparse JBIG2 regions exceeded a payload-derived budget | `hayro-jbig2/src/decode/generic.rs` | `region_iteration_budget` | Reproduced: a 2550×3300 region with one blob |
| `12.5-` split into two operands; underfilled path operators drew to the origin | `object/number.rs`, `content/mod.rs` | content tokenizer and `path_operands` | GHOSTSCRIPT-687395-0 (diff 0.36 → 0.006) |
| A form drawing itself through a pattern fanned out to the 500k-command budget | parent-chain tracking in `object/indirect.rs` | `active_forms` re-entry check | GHOSTSCRIPT-700301-0 |
| A high-resolution `/SMask` or `/Mask` was shrunk to the image's resolution | `hayro/src/image.rs` (mask drawn at its own resolution) | `fold_alpha_plane` upsamples the image instead (single-pass fixed-point bilinear) | Reproduced: 1×1 image under a 300×300 mask. Cost: PDFBOX-3948-0 ≈300 → ≈505 ms at 72 dpi; capping the upsample at the image's device size is a follow-up |

poppler-660/661/664 also moved closer to hayro (diff 0.15 → 0.026). Their
page content only draws a missing XObject, so the change comes from an
annotation appearance; which of the fixes above is responsible was not
isolated.

Every row has a regression test. The workspace passes `cargo test`,
`cargo clippy --all-targets` with no warnings, `cargo fmt --check`, and a
`wasm32-unknown-unknown` check of `zpdf-wasm`.

0.19.0 (upstream, independently) already moved to hayro-jpeg2000 0.5 in
strict mode and rejects codestreams truncated inside their tile data, so
strict exports cannot publish an incomplete image. The JPX fixes here keep
that policy.

## 5. Remaining gaps, ranked

Effort: S < 1 day, M ≈ several days, L ≈ a week or more.

### P1: visible on real documents

1. **Tiling patterns as a repeated paint (L; M for CPU only).**
   - Problem: zpdf re-interprets the cell for every tile. Above 4096 tiles it
     substitutes a solid colour, and dense cells hit the command budget.
   - Corpus: MOZILLA-1244729-0 (map regions filled black and cyan),
     PDFBOX-1094-5 (page truncated at 500k commands).
   - hayro: renders the cell once into a pixmap and repeats it as an image
     paint (`hayro/src/tiling.rs`).
   - Plan: add a `Paint::Tiling` that carries the cell's display list, as
     `SoftMask` already does. In the CPU backend, use a tiny-skia
     `Pattern` with `SpreadMode::Repeat`. Keep per-tile replication as the
     GPU fallback.
2. **Type 3 glyph fidelity (M).**
   - Gap: glyph procedures run without the font's `/Resources`. `d0` (coloured)
     glyphs are forced to the fill colour. Commands other than fill, stroke
     and image are dropped.
   - hayro reference: `hayro-interpret/src/font/type3.rs`.
3. **Unloadable embedded font becomes invisible text (S).**
   - When neither parser accepts the program, the font is widths-only and the
     run is skipped.
   - hayro instead substitutes a standard font chosen by flags and name
     (`hayro-font/src/type1.rs`, `true_type.rs`).

### P2: correctness and quality

- **Standard-14 outlines for WASM and font-less hosts (M).**
  - hayro bundles 14 Foxit PFBs (about 240 KB, BSD licence from PDFium).
  - zpdf has metrics only, so non-embedded Helvetica draws nothing unless the
    application registers fonts.
  - Needs a maintainer decision on the licence.
- **Horizontal stretch of substitute glyphs to `/Widths` (M).** hayro:
  `standard_font.rs`, `cid.rs`. zpdf's `PositionedGlyph` has no per-glyph
  scale.
- **CFF `FontMatrix` (S/M).** Top-level and per-FD matrices are ignored
  because ttf-parser exposes the matrix without applying it.
- **Shadings (S each):**
  - Type 1 function-based shading is unsupported.
  - Meshes with `/Function` should interpolate the parameter, not RGB.
  - `/Background` and `/BBox` are ignored.
  - Rasterization caps at 768 px regardless of DPI.
  - Native gradient paints (M) would also give SVG and PPTX real gradients.
- **Strokes under a non-uniform CTM (S–M).** Width and dashes are averaged
  into one scale, so stroke width is wrong on squashed ellipses.
- **Clip elision for rectangles that cover the current bounds (S).** zpdf's
  own §15.9 measured 1,324 PushClips on test7.
- **ExtGState `/TR` and `/TR2` transfer functions (S–M).**
- **`/Interpolate false` → nearest when magnifying (S–M).** QR codes and
  barcodes would stay crisp. zpdf deliberately matches pdfium's bilinear
  sampling today, so this needs a maintainer decision.
- **JPX `target_resolution` (M).** Reduced-resolution decode for thumbnails.
  It is already available in hayro-jpeg2000.

### P3: robustness, speed, size

- **JBIG2 (S each):**
  - Unknown-height (0xFFFFFFFF) generic regions.
  - EXTTEMPLATE.
  - Symbol-dictionary context-used/retained flags.
  - Skip, rather than fail on, an out-of-range symbol ID (poppler-28806-0).
- **JBIG2 decode speed (M–L).** Packed words and MPS runs, as in hayro.
  GHOSTSCRIPT-693513-4 spends 440 ms decoding a blank 14 Mpx image.
- **JBIG2 differential oracle (S).** Using hayro-jbig2 as a test-only oracle
  is cheaper than replacing zpdf's decoder. Replacing it would lose
  `ParseLimits` budgets and per-region failure isolation.
- **CCITT:** Group 3 EOL resync and alignment handling. hayro-ccitt (about
  1.2k lines, no dependencies, forbids unsafe) is a candidate to adopt.
- **DCT:** `/ColorTransform 0`, and patching frame dimensions from the
  dictionary.
- **Content tokenizer:**
  - Borrowed operators and fast integer parsing; the probe showed about 4×
    tokenizer throughput as an upper bound.
  - `resolve()` deep-clones on cache hits; move hot paths to
    `resolve_shared`.
  - Filtered inline-image `EI` detection.
  - The repair scan parses every object body.
- **SVG export:** glyph `<use>` deduplication (hayro-svg `Deduplicator`).
- **CMaps:**
  - Plain `H`/`V` (Adobe-Japan1) are missing, which needs `maps.bin`
    regenerated.
  - The legacy GBK/Big5/KSC/SJIS/EUC-JP tables (about 300 KB in WASM) look
    unreachable.
  - `maps.bin` is 2.4 MB uncompressed in WASM; hayro's brotli blob is 250 KB.
- **Rasterization:** an aliasing-threshold option, and rounding instead of
  truncating 8-bit colour and mask products.
- **Glyph raster cache:** earlier zpdf attempts were net negative. hayro's
  layout (4 x bins, integer y, glyphs ≤ 128 px) is untested here and worth one
  experiment.

## 6. minipdf: what to take

- **Text shaping and bidi in authoring (M).**
  - `DocumentBuilder::add_text_with_fonts` explicitly does no shaping or bidi.
  - minipdf shapes with rustybuzz and reorders with unicode-bidi (both pure
    Rust, see `pdf.rs` `shape_text`). It writes `/ToUnicode` per cluster, so
    ligatures map back to every character they cover.
  - This is the one feature that would let zpdf-writer produce Arabic,
    Hebrew, Indic and Thai text, plus kerning and ligatures for Latin.
- **CFF/CFF2 subsetting (S–M).** The `subsetter` crate (pure Rust, used by
  Typst) handles both. zpdf only blanks CFF charstrings and leaves CFF2
  untouched.
- **Run splitting with a cached coverage probe.** zpdf's `FontProvider` already
  does per-character fallback, so there is little to add.
- **Not recommended:**
  - minipdf's fixed font-descriptor values (`/Flags 32 /StemV 80`).
  - Taking on Office→PDF conversion. It needs a layout engine and is a
    separate product.
  - A PPTX→PDF path to round-trip with zpdf-pptx-export would be the only
    natural overlap.
- **Scored visual reports.** minipdf's benchmark scores against reference
  renderers and emits heatmaps, which matches the harness used here. Checking
  that harness in as a detached crate (like `fuzz/`) would make §3
  repeatable.
