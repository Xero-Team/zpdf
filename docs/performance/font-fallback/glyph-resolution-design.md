# Coverage resolution integration plan

Work package: [#50](https://github.com/Xero-Team/zpdf/issues/50).
Status: mapping foundations and provider coverage selection implemented;
page/run, union/empty clipping and horizontal scalar writer integration
implemented. Latest performance gates remain pending.

## Namespace and ownership

Keep the PDF font as the logical source of code segmentation, text extraction,
widths, spacing and vertical origins. The drawing font may differ. A resolved
glyph therefore needs an actual page `FontId` and a GID in that font, while its
position and advance remain those computed from the PDF font. Existing
composite drawing IDs can be CIDs; convert only when selecting another face.

Expose coverage selection from the provider so writer authoring and content
interpretation can share it. A successful result should retain a stable
registered identity, bytes, face index and actual GID. Do not use font names
as identities or treat a successful family-name match as character coverage.
Probe registered faces in deterministic preferred/style order; consider lazy
packs only after their coverage hints permit the character, and validate the
decoded cmap. TTC faces share bytes but retain distinct face identities.

## Preserve the existing fast path

Avoid registry locking, hashing full fonts, allocation or cmap decoding for
each glyph in documents without an explicit provider. Allocate optional
resolution state only when the provider path is enabled. Original usable
glyphs should keep the existing face. Never decide that a valid space is
missing solely because `outline_glyph` returns no path.

Cache successful and unsuccessful character resolution in bounded storage,
keyed by logical font identity plus full Unicode selector semantics. A
provider generation change must invalidate misses and changed selections;
already-emitted font IDs remain stable for old display lists. When a cache
fills, clear or stop caching decisions, not the actual font store. Admission
of fallback font programs must respect the page's font-cache byte budget.

## Text operation integration

During code decoding, retain reliable Unicode only where fallback is enabled.
Use `/ToUnicode` before encoding-derived Unicode; for UTF-16 codes keep the
raw code and decoded scalar distinct. Identity CID values alone do not prove
Unicode. Preserve multi-scalar extraction strings. Explicit policies and
tests are needed for variation sequences, ligatures and PUA mappings; do not
globally shape text that PDF has already positioned.

After computing original positions and extraction, release the immutable
logical-font borrow, admit selected drawing faces and replace drawing IDs.
Split adjacent glyphs only when their actual font changes. All resulting
runs retain the same original transform, rise, scale, paint and alpha.
Advancing the text matrix still happens once per original operation.

For text clipping modes, accumulate outlines from all selected faces into the
same text-object clip union. Pattern fills also need one union clip and one
pattern-paint operation: painting overlapping fallback runs separately can
change alpha. Invisible text must preserve extraction and matrix advancement.
Forms inherit the same provider; original resource names must not collide
with synthetic drawing-face IDs.

## Authoring and cross-backend validation

Provide an additive multilingual writer path that resolves Unicode coverage,
embeds each selected face/subset and emits adjacent runs at correct positions.
Preserve existing single-font writer APIs. TTC support requires extracting a
chosen face as an independent sfnt or explicitly extending the writer's face
index handling; passing a TTC unchanged to a face-zero-only writer is wrong.

Before final acceptance, validate horizontal and vertical fallback under
rotation, scaling, text rise, TJ adjustments, clipping, forms, repeated misses
and late registration. CPU, GPU and SVG must consume the same drawing IDs.
The GPU now includes positioned `glyph.y` in outline and Type3 paths, with
actual-adapter placement checks. Predefined-CMap CID mapping in #51 updates
the reader and writer together; source-backed tests distinguish code/CID/GID.

Benchmark the new recovery workloads separately from unchanged controls:
cold resolution, warm hits, repeated misses, mixed-face runs, clipping, and
writer round trips. Every workload must first assert glyph identity,
extraction, PDF advances/positions and visible output. Full acceptance still
requires the original same-host control suite and retained raw evidence.

## Reusing original-glyph checks

Cold CJK setup exposed duplicate outline validation. Mapping preserves a GID;
drawing resolution validates its usability. A text operation shares a single
parsed face, and the provider reuses results for immutable LoadedFont identities.
Weak references prevent address reuse without retaining font programs. Results
are grouped by font and bounded by 4,096 glyphs and 1,024 font identities,
including empty/missing fonts. Copies with different maps, TTC faces or axis
settings cannot share an identity accidentally.

Existing per-page resolution decisions take priority over shared validation.
An operation-wide check is useful when all originals are usable, but must be
skipped once a mixed-run page cache exists. Reversing that order caused measured
6–17% mixed-page regressions and was rejected.

The original-check map is allocated lazily inside the registry's existing lock.
The character-coverage hash map stays directly accessible. Cache layouts that
increased name-only registration cost or indirected warm coverage were measured
and rejected. Current source-wide acceptance remains separate from these
focused improvements; see STATUS.md for exact binary/report identities.
