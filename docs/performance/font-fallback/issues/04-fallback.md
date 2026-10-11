# fix(font): resolve missing glyphs by coverage while preserving PDF placement

## Problem or use case

Selecting one substitute face does not recover individual missing characters. Some failed mappings reuse a raw character code as a GID, which can draw an unrelated glyph. A GlyphRun currently references one font, so fallback must explicitly select the actual drawing face.

## Proposed behavior

Resolve usable original glyphs first, then select a compatible fallback face using reliable Unicode/collection mappings and coverage. Split runs as needed while preserving original advances, vertical origins, transforms, spacing, clipping and extraction. Cache successful and unsuccessful resolution with provider-generation invalidation and bounded memory.

## Acceptance criteria

- Mixed Latin/CJK/supplementary characters and a missing symbol can select different faces within one original PDF text operation.
- A missing mapping cannot silently become an unrelated substitute GID.
- Empty valid glyphs such as spaces are not treated as broken outlines.
- Multi-scalar mappings and variation selectors have explicit, tested semantics; original positioned PDF text is not globally reshaped.
- Forms, text clipping, CPU/GPU/SVG and text extraction agree on actual glyph selection.
- Writer APIs can use the same coverage selection when creating multilingual text and embed each selected subset.
- Benchmarks cover cold misses, warm hits, repeated misses, mixed runs and unchanged embedded text.

Depends on the font-pack sub-issue.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

Implemented per-character registered coverage, lazy-pack coverage hints,
style matching and bounded positive/negative decisions. Unmapped codes do not
become unrelated GIDs. Valid spaces, full extraction strings and explicit
variation selectors retain their semantics. Missing Type0 programs retain
segmentation, CMaps, W/W2 and original placement.

Page resolution selects an actual drawing face and splits adjacent runs while
preserving PDF advances, vertical origins, TJ/spacing, transforms and rise.
Existing page decisions take priority over shared original checks. Those
checks use Weak LoadedFont identities, retain no font bytes, and are bounded
by 4,096 glyphs and 1,024 identities. Provider generations invalidate misses
without invalidating existing display lists.

CPU, GPU and SVG tests verify mixed-face placement, union/pattern text clips,
empty clips and graphics-state restoration. Actual Apple M5/Metal checks use
MSAA x4; pixel-identical CPU/GPU antialiasing is not claimed. Repeated simple
codes reuse only mapping/advance decisions, with tests against individually
positioned Tj operations. Distinct variation sources cannot alias.

The scalar multilingual writer embeds each selected face, extracts TTC faces
and preserves rounded emitted widths at run boundaries. Unsupported scalars
or selectors fail atomically. This API is horizontal scalar authoring, without
shaping or bidi reordering.

Adjacent-repeat measurements, with current binary bindings noted below:

- Coverage: all eight pass. Cold hit -7.95%, cold miss -37.91%, late registration
  -2.52%, warm variation -2.07%, warm hit -1.97%, warm miss -4.98%.
  Report: font-repeat-final-coverage-paired.json.
- Page fallback: all nine pass. Cold setup -1.02%, warm setup +1.12%, warm page
  cache +0.56% (upper +2.68%), CPU render +0.27%, text clip +2.22% (upper +3.40%)
  and vertical setup -2.20%. Extraction, SVG and Form controls also pass.
  Report: font-repeat-final-fallback-paired.json.
- Provider: all eight pass for the same frozen source. Repeated-A page setup
  improves 64–72%, with unchanged glyph/position assertions.
- The newer writer preflight candidate passes all three writer controls:
  repeated unsupported text -50.27%, mixed authoring +2.45% (upper +3.02%) and
  single-face authoring +2.01% (upper +2.79%). Its coverage and page-fallback
  executables are byte-identical to the captures above; provider and all other
  changed functional binaries have also completed fresh comparisons and pass.
  All 125 functional controls pass; the pure-copy diagnostic remains open in #52.

Writer preflight resolves complete coverage before metric/layout work. A new
overflow test verifies rollback of a newly admitted face, preservation of
existing content and correct later reuse. Current broad validation reports
847 passes across 46 suites, lint and actual WASM compilation.

All comparisons use 21 alternating AB/BA pairs and the strict upper-95%-bound
+5% gate. Earlier measured regressions and uncertain candidates remain
archived; their passes are not transferred to this binary. The user selected
the Linux environment as the final performance acceptance platform, where the
font and pipeline inventories pass; the retained macOS pure-copy diagnostic is
a documented platform-specific limitation.
