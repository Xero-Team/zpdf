# zpdf Repair Audit Revision

## Document control

| Field | Value |
|---|---|
| Project | zpdf |
| Workspace version | 0.10.0 |
| Audit revision date | 2026-07-13 |
| Audited base | `026e5ba` (`v0.10.0`) |
| Scope | All 15 workspace crates plus the detached fuzz crate |
| Change set at verification | 53 tracked files, 6,124 insertions, 1,345 deletions |
| Status | Repairs implemented and verification complete |

This document is the current repair record for the project. The other reports in
`docs/review` dated 2026-07-10 are retained as historical audit-session notes;
their pending-item lists are superseded by the closure information below.

## Executive summary

A comprehensive project-wide static, semantic, and regression audit was
performed across the parser, document model, content interpreter, font and image
decoders, color management, CPU and GPU renderers, viewers, CLI, and incremental
writer.

The repair concentrated on four outcomes:

1. Improve PDF rendering throughput and eliminate avoidable full-buffer copies.
2. Bound allocations, recursion, cache retention, and GPU/CPU working sets for
   untrusted PDF input.
3. Correct malformed-input, overflow, state-balancing, color, image, font, xref,
   and writer failure cases.
4. Preserve graceful degradation: reject, truncate, skip, downscale, or fall
   back instead of panicking, hanging, corrupting output, or exhausting memory.

All findings raised during the final combined-diff review were repaired. The
final workspace build, strict lint, full test matrix, GPU/CPU parity suites, and
fuzz-crate compatibility check pass.

## Audit scope and method

The review covered the following crates:

- `zpdf-core`
- `zpdf-parser`
- `zpdf-document`
- `zpdf-content`
- `zpdf-display-list`
- `zpdf-font`
- `zpdf-image`
- `zpdf-color`
- `zpdf-render`
- `zpdf-render-cpu`
- `zpdf-render-wgpu`
- `zpdf-viewer-gpui`
- `zpdf-cli`
- `zpdf-writer`
- the `zpdf` facade and its integration tests

The audit combined:

- subsystem-by-subsystem source review;
- cross-crate resource-lifetime and limit propagation review;
- combined-diff semantic regression review;
- searches for unchecked arithmetic, panic paths, unbounded collections,
  recursion, allocation amplification, and stale cache lifetimes;
- focused regression tests for each repaired class of defect;
- strict Clippy with warnings denied;
- complete workspace tests, builds, CPU/GPU parity tests, and fuzz-crate checks.

This is an engineering audit and regression verification record, not a formal
proof that every possible malformed PDF is harmless.

## Repair summary by subsystem

### Core, parser, and xref handling

- Made `PdfDict` string lookup allocation-free through borrowed `str` keys.
- Added finite-matrix validation and rejected non-finite inversions.
- Enforced cumulative decoded-stream budgets across filter chains, including
  streams with no filter.
- Propagated active `ParseLimits` instead of silently reverting to defaults.
- Added byte-budgeted admission for resolved-object and decoded-object-stream
  caches. Resolution still succeeds when a cache is full; the object is simply
  not retained.
- Checked object-stream `/N`, `/First`, member offsets, range ordering, and all
  allocation arithmetic.
- Validated xref field widths, object IDs, generation numbers, offsets, ranges,
  `/Prev` traversal, and cross-platform `u64` to `usize` conversions.
- Corrected sparse xref handling: `max_objects` now limits processed entry count,
  not the highest sparse object number.
- Improved raw `startxref` discovery and damaged-xref recovery without depending
  on valid UTF-8 or a final 1 KiB window.
- Bounded recovery scans and memoized their result.
- Validated JPEG dimensions against the active image-pixel limit before decode.

### Document model and content interpretation

- Bounded page-tree depth and total pages, page content arrays, annotations,
  optional-content groups, and aggregate resource entries.
- Bounded combined decoded page-content bytes before concatenation.
- Enforced operand, graphics-state, marked-content, command, operator, form,
  pattern, mesh, shading, and wall-clock work limits.
- Bounded retained tokenizer strings, arrays, dictionaries, and recovery state.
- Added checked shading and mesh allocation arithmetic.
- Prevented non-finite transforms and color values from entering display lists.
- Avoided decoding image XObjects, inline images, and rasterized shadings when
  extraction callers intentionally omit an image cache.
- Memoized rejected image objects so a page does not repeatedly decode an image
  that cannot be admitted to the cache.
- Preserved graphics-state, marked-content, clip, and blend structure when work
  is truncated by a limit or deadline.

### Fonts

- Hardened CFF INDEX/offset parsing, Type 1 execution, subroutine lookup,
  `seac` recursion, PostScript stacks, CMap tokens, mappings, and range
  expansion.
- Removed per-subroutine cloning and bounded sparse tables and interpreter work.
- Corrected UTF-16 surrogate-pair CMap handling while preserving raw codes for
  `ToUnicode` lookup.
- Removed a hot-path temporary `String` allocation from glyph-name lookup.
- Changed the global system-font byte cache to weak references so unused font
  files can be released instead of being retained for the process lifetime.
- Corrected `FontCache` lifetime semantics: display lists retain `FontId`, so
  evicting a referenced entry was unsafe. IDs now remain stable and document
  loading uses non-evicting byte-budgeted admission.
- Deduplicated shared font-program accounting and prevented ID wrap from
  overwriting live entries.

### Images and color management

- Enforced active `max_image_pixels` limits for raw, DCT/JPEG, JPX/JPEG 2000,
  image-mask, soft-mask, and stencil-mask paths.
- Rejected invalid, wrapped, zero, and overflowing dimensions and unsupported
  bits-per-component values before allocation.
- Validated exact decoded sample sizes and JPEG component layouts.
- Added full Lab image conversion and Bradford adaptation from a PDF Lab white
  point to the D65 white used by sRGB.
- Chunked ICC raster conversion to bound temporary component, mask, RGB, and
  RGBA planes.
- Capped ICC profile input and cache growth and memoized failed compilation.
- Precomputed Indexed palettes, including ICC-backed palettes.
- Applied same-size masks and alpha directly in place, avoiding temporary full
  alpha planes.
- Added non-evicting image-cache byte admission using retained `Vec` capacity,
  stable IDs, and wrap-safe ID allocation.
- Rejected non-finite PDF Function results even when `/Range` is absent, and
  bounded sampled, stitching, and PostScript function parsing/evaluation.

### CPU rendering

- Transfers the final tiny-skia pixel buffer directly instead of cloning the
  full page raster.
- Enforces the document page-pixel limit before allocation. Oversized CPU pages
  are uniformly downscaled to an exact ceil-rounded pixel cap.
- Bounds clip-mask memory, cumulative clip-pixel work, live blend surfaces,
  blend depth, recursive soft masks, soft-mask cache bytes, and downscaled-image
  cache bytes.
- Shares soft-mask coverage through `Arc<[u8]>` and reuses repeated masks.
- Caches box-filtered image minifications used repeatedly by patterns/XObjects.
- Validates path extents, images, alpha, and page geometry before handing work
  to raster primitives.
- Unwinds open groups and preserves matching structural pops when the deadline
  or a resource limit is reached.
- Fixed dashed zero-length painted intervals so round-cap dot patterns are not
  lost while still preventing non-progress loops.

### GPU rendering

- Reuses GPU contexts in viewers instead of rebuilding the device and pipelines
  for every page.
- Allocates page render attachments lazily after the display list reveals
  whether the single-pass or layered path is required.
- Enforces page-pixel, total GPU texture/readback, image texture, glyph atlas,
  transparency-layer, and blend-depth budgets.
- Uses saturating byte estimators for adversarial dimensions.
- Makes timestamp telemetry opt-in so ordinary rendering avoids its extra map,
  poll, buffers, and synchronization point.
- Fixed image batching indices and prevented glyph-atlas eviction from
  overwriting slots still referenced by recorded quads.
- Allocates glyph atlas CPU pixels lazily and uploads only the used extent.
- Validates images, quads, paths, clips, alpha, and nested soft masks before
  recording or allocating GPU resources.
- Preserves balanced blend/clip state and degrades excess groups to passthrough
  or source-over behavior.

### CLI and viewer

- PNG output now writes the rendered RGBA buffer directly instead of cloning it
  into a second image buffer.
- Edit commands parse the PDF once and reuse the writer's parsed document for
  signature warnings.
- Text/table extraction no longer allocates a page-index vector for `--all` and
  does not create image caches when image output is unnecessary.
- Validates positive page numbers, finite positive DPI, page ranges, rotations,
  stamp page numbers, and selected-page expansion limits.
- Avoids compare arithmetic overflow and handles zero-pixel images.
- Bounds attachment collection and removes partially written extraction files.
- Reuses the GPUI GPU context, discards unhealthy contexts, and falls back to
  the bounded CPU renderer when appropriate.

### Incremental writer

- Stores pending stream bytes as `Arc<[u8]>` and avoids an extra clone for
  already-compressed buffers.
- Serializes objects and streams directly to the output writer instead of
  staging large stream payloads in another `Vec`.
- Converts serialization panics into `io::Result` errors and rejects non-finite
  reals and illegal nested direct streams.
- Validates trailer `/Size` against all known object IDs, preventing collisions
  caused by stale or malformed trailers.
- Adds fallible object, stream, and Flate-stream allocation APIs and preflights
  multi-object edits near object-number exhaustion.
- Makes annotation, form, metadata, and stamp operations transactional with
  respect to object allocation failures.
- Uses 8-byte xref-stream offsets and rejects offsets that cannot be represented
  by the classic ten-digit xref format.
- Scans the whole raw file for `startxref`, matching parser behavior for PDFs
  with long trailing appends.
- Corrects stamp resource-name collisions and validates geometry, colors,
  raw-buffer sizes, JPEG SOI/SOF metadata, components, dimensions, and pixels.
- Fixes page deletion validation, count arithmetic, cycle/depth handling, and
  button appearance-state case preservation.

## Resource limits now enforced

The document-driven pipeline now consumes the active `ParseLimits` values
through parsing, interpretation, image/font admission, and rendering.

| Limit | Default | Enforcement summary |
|---|---:|---|
| `max_object_depth` | 100 | Parser nesting |
| `max_stream_bytes` | 256 MiB | Raw stream lengths |
| `max_decoded_stream_bytes` | 256 MiB | Cumulative filter output and page content |
| `max_image_pixels` | 100,000,000 | JPEG, JPX, raw images, and masks |
| `max_page_operators` | 1,000,000 | Content interpreter work |
| `max_string_length` | 16 MiB | PDF strings/names and retained content strings |
| `max_objects` | 5,000,000 | Recovery/xref entry work |
| `max_operand_stack_depth` | 10,000 | Content operand stack |
| `max_graphics_state_depth` | 256 | `q`/`Q` retained state |
| `max_marked_content_depth` | 128 | `BMC`/`BDC` nesting |
| `max_blend_group_depth` | 16 | CPU/GPU groups and soft-mask nesting |
| `max_object_cache_bytes` | 512 MiB | Parsed object cache admission |
| `max_objstm_cache_bytes` | 256 MiB | Decoded object-stream cache admission |
| `max_image_cache_bytes` | 1 GiB | Decoded page image admission |
| `max_font_cache_bytes` | 256 MiB | Retained unique font programs |
| `max_softmask_cache_bytes` | 512 MiB | CPU soft-mask coverage cache |
| `max_page_pixels` | 64,000,000 | CPU/GPU output raster |
| `max_gpu_texture_bytes` | 1 GiB | GPU target, readback, images, atlas, and layers |

Additional internal caps remain intentionally in place as defense in depth for
clip work, blend working surfaces, recursion, resource arrays, pattern tiling,
font programs, ICC profiles, and stamp inputs.

## Closure of the 2026-07-10 action items

| Previous item | Revised status | Resolution |
|---|---|---|
| H1 filter-chain decompression bomb | Complete | Cumulative decode budget enforced across filters and unfiltered data |
| H2 JPEG header validation | Complete | Header dimensions/components validated before decode; active pixel limit used |
| H3 xref overflow protection | Complete | Checked fields/ranges/offsets, sparse xrefs, and entry budgets |
| H4 parsed object cache | Complete | Byte-budgeted non-retaining fallback |
| H5 image cache | Complete | Byte-budgeted stable-ID admission and rejected-object memoization |
| M1 font cache | Complete | Stable IDs, byte-budgeted admission, weak system-font storage |
| H6 operand stack | Complete | Active document limit enforced |
| H7 graphics/marked-content nesting | Complete | Active document limits enforced with balanced degradation |
| H8 blend groups | Complete | CPU/GPU depth and working-set limits |
| H9 soft masks | Complete | Shared planes, recursion guard, byte-budgeted cache |
| H10 page/GPU allocation | Complete | Page-pixel and unified GPU texture/layer budgets |
| M2 tile/work amplification | Complete | Command/operator/deadline checks in recursive emitters |
| M3 GPU texture budget | Complete | Images, atlas, target/readback, and layers included |
| M5 limit consistency | Complete | Document limits flow through public document-driven paths |
| L1 ID counters | Complete | Wrap-safe caches and fallible writer allocation paths |
| L2 image dimensions | Complete | Checked dimensions, samples, channels, masks, and stamp metadata |

## Compatibility and degradation notes

### `FontCache` constructor semantics

The previous capacity-based eviction behavior could invalidate `FontId` values
already stored in a display list. `FontCache::with_capacity` is therefore kept
only as a deprecated compatibility shim and now acts as a preallocation hint.
New code should use `FontCache::with_preallocated_capacity`. Document loading is
bounded with byte-budgeted admission rather than unsafe eviction.

### Oversized pages

- CPU rendering uniformly downscales to `max_page_pixels` so the complete page
  is still returned within the allocation budget.
- GPU rendering returns `Unsupported` when page or texture budgets are exceeded.
  The GPUI viewer then falls back to the bounded CPU path.

### Excess nesting or work

- Excess blend groups render as passthrough/source-over rather than allocating
  another full-page intermediate surface.
- Excess clips, masks, patterns, forms, states, or operators are skipped or
  truncated while matching structural pops continue to unwind safely.
- Malformed/oversized images, fonts, functions, streams, and writer inputs are
  rejected with errors or warnings instead of panics.

### GPU timing

GPU timestamp timing is disabled by default. Call
`WgpuRenderer::with_gpu_timing(true)` only when telemetry is required.

### Legacy infallible insertion APIs

Legacy cache/writer insertion methods remain for source compatibility. Normal
document and high-level writer paths use fallible admission/allocation. The
legacy methods can only panic after exhausting the entire usable `u32` ID space,
which is not reachable through the bounded document pipeline.

## Verification record

The final state was verified with the following commands:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
cargo build --workspace --all-targets
cargo check --manifest-path fuzz/Cargo.toml
git diff --check
```

Results:

- Formatting: passed.
- Strict Clippy: passed with warnings denied.
- Workspace all-target tests: 921 passed.
- Documentation tests: 5 passed.
- CPU/GPU corpus, markup, overprint, soft-mask, and blend parity tests: passed.
- All workspace targets: built successfully.
- Detached fuzz crate: checked successfully.
- Patch whitespace validation: passed.

No benchmark targets currently exist in the repository, so the performance work
was verified through removal of full-buffer copies, cache/context reuse, lazy
allocation, batching behavior, focused regression tests, and the complete render
parity suite rather than through a repository benchmark harness.

## Final assessment

The repair revision closes the actionable findings from both the earlier July 10
security review and the final project-wide regression review. The codebase now
has substantially stronger allocation discipline, stable resource lifetimes,
safer malformed-input handling, lower avoidable rendering-copy cost, and
document-configurable CPU/GPU resource control.

At the time of this revision:

- no known audit finding remains open;
- no workspace test, strict lint, build, parity, or fuzz-compatibility check is
  failing;
- the repair changes are present in the working tree and have not been committed
  by this audit session.

