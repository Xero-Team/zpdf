# Font coverage implementation and benchmark report

This is a working-tree report for issues #46–#52. Performance acceptance is
complete for the user-selected Linux platform: the 57-case font inventory and
the repaired 135-case pipeline inventory both pass the unchanged 5% gate.
Source, fixtures and executable captures are retained, and the campaign is
published as a pull request linked from issue #46; no issue is closed by it.
See [STATUS.md](STATUS.md) for the latest measurement state and
[HISTORY.md](HISTORY.md) for prior candidates.

The preceding fully measured candidate combines narrow-table scalar lookup, bounded
original-glyph checks using the shared registry lock, page-decision priority,
and adjacent equal-code reuse. Its complete comparisons are archived:
129 pass, six inconclusive and one copy diagnostic
regression after fixed confirmations. The current candidate moves writer
coverage validation before metric/layout work, avoiding prefix metrics when
an unsupported suffix will fail. Its completed captures and measurements bind
the exact source to executable bytes; see [STATUS.md](STATUS.md).

The current candidate completes all 125 functional font performance controls
with a pass. After all fixed precision comparisons, the complete 136-case
inventory reports 135 pass and one classified pure-copy regression, with no
inconclusive cases. It is still not accepted as a complete gate.
[PRECISION_MEASUREMENTS.md](PRECISION_MEASUREMENTS.md) and
[the evidence export](evidence/writer-preflight-precision-pairs.json) retain every current
paired observation, source/executable binding and unsuccessful outcome.

Further diagnostic work corroborates executable-specific macOS allocation
bucketing and shows that changing only the source allocation's bucket changes
pure-copy cost substantially in both archived binaries. The fixed experiments
retain all data and do not replace acceptance measurements. This identifies
an allocator contribution while leaving the 135-pass / one-regression gate
open; see [the full investigation](copy-diagnostic-investigation.md).

The user-provided Linux container now supplies independent validation against
production baseline 137ab2b with matched workloads and third-party dependencies.
Native tests report 788 passes. All 46 original font controls pass after fixed,
retained-data precision comparisons, including mixed-supplementary extraction
at +4.02% (upper +4.53%). All eleven diagnostics pass, including copy_heap
at -0.13% (upper +1.01%) and copy_aligned at +3.30% (upper +3.68%, 1,109
retained pairs). All 57 common Linux controls therefore pass. Source and
executable hashes are unchanged, and all 60,200 raw samples are archived.
See [the Linux recheck](LINUX_RECHECK.md). These
results neither recreate the intermediate macOS baseline nor replace its data.

The separate original synthetic pipeline comparison now completes 21 fixed
full-suite pairs: 131 pass, four CPU-render cases inconclusive, zero classified
regressions. All 27 end-to-end cases pass, with a largest upper bound of +2.67%.
The helper now honors explicit Criterion CLI budgets on both sides; timed
operations and fixtures are unchanged. An independent audit verifies all
57,120 raw samples and the archived binaries. That original SmallInput
protocol retains its four uncertain cases; the repaired protocol below passes
the complete matrix. See
[PIPELINE_RECHECK.md](PIPELINE_RECHECK.md) and
[PIPELINE_MEASUREMENTS.md](PIPELINE_MEASUREMENTS.md).

A separate fixed output-retention diagnostic reproduces one failed SmallInput
same-binary calibration and passes all four PerIteration calibrations. The
3,360 raw samples and unchanged production identities are verified and archived.
This identifies a measurement concern involving live RGBA outputs, not a
production optimization or a substitute for before/after acceptance. The original
pipeline result remains 131 pass and four inconclusive.

Applying PerIteration identically to both stages targets and repeating the
complete comparison now passes all 135 cases, with no uncertainty or classified
regression. The largest upper 95% bound is +4.11%. All 58,380 raw samples,
84 process markers and exact identities were independently audited and archived
on both hosts. This separately repaired protocol is never pooled with the
original SmallInput data. See [the final pipeline table](PIPELINE_BOUNDED_MEASUREMENTS.md).
The retained macOS heap-copy diagnostic is documented as a platform-specific
limitation rather than a blocker for the selected Linux acceptance platform.

A separate same-allocation-callsite probe also fails to establish stable
same-binary timing: all four fixed calibrations are inconclusive. Its 1,680
samples, binaries and assembly are retained in [copy-callsite-results.md](copy-callsite-results.md).
That proposed benchmark modification is not adopted. Linux acceptance is
complete for the measured inventories.

The subsequent fixed precision extension retains all first-run samples and
passes all four probe calibrations with 512 pairs per condition. The full audit
checks 40,960 samples. Both shared and distinct allocation modes pass, so this
does not establish a specific advantage for changing the original benchmark.
It is not a comparison of the original font binaries and does not clear their
retained regression. See [the precision results](copy-callsite-precision-results.md).
All planned runs are complete. The user selected the Linux environment as the
final performance acceptance platform; the retained macOS heap-copy diagnostic
does not block that decision and is not excluded, relabeled or cleared.

Local English reports and issue-body sources contain the final precision
result and the acceptance decision. The user authorized updating the GitHub
issue bodies with the measured performance data and without local paths.

## Resulting behavior

Applications can register shared TTF/OTF/TTC bytes or bounded raw/zlib packs.
The host can explicitly download a pinned font, verify its digest, cache it
and reuse it offline. Lazy packs decode/admit once, share concurrent requests,
cache failures, and reserve decoded capacity before decompression. Rendering
does not perform networking or repeatedly decompress a font for each glyph.

Missing glyphs use registered coverage while retaining the PDF's advances,
spacing, transforms and vertical origins. Original usable glyphs and valid
empty spaces are preserved. Page decisions and provider coverage decisions
are bounded and invalidated when registration changes. Drawing-face identities
include the logical font, preserving descriptor variation settings. Multi-face
text clipping is accumulated as one union; empty text clips block later paint
and restore correctly across CPU, GPU and SVG.

PDF Encoding CMaps and font-file cmaps serve different namespaces. The former
map encoded source bytes to CIDs; the latter map Unicode to physical glyphs.
84 pinned Adobe CMaps and five canonical collections now provide the PDF
mapping layer. Character-code lengths, usecmap overrides, four-byte GBK2K,
HKSCS, supplementary UTF-16 and full canonical strings are retained. Identity
does not imply Unicode. Known Adobe collections can recover text without
ToUnicode, and explicit ToUnicode takes precedence.

Writer fallback embeds/subsets the selected faces and uses rounded emitted
widths at run boundaries. Predefined authoring keeps source codes separate
from the collection CIDs used by W/W2 and TrueType CIDToGIDMap. CID-keyed CFF
retains its charset and ROS metadata; incompatible predefined collections or
charsets fail explicitly. Vertical writing uses upright glyph matrices,
normalized vmtx advances and VORG/bearing-derived origins. Missing vertical
metrics use PDF defaults. TTC/sfnt rebuilding bounds aggregate table copying
and recomputes checksums.

## Measured optimizations

- Separate optional painting/clipping from the ordinary decoding stack; avoid
  per-glyph Arc churn and redundant lookups; reuse adjacent equal-code decisions.
- Check the page-local GPU atlas before outline extraction. Device-size keys,
  independent x/y scales, actual drawing IDs and the existing vector fallback
  remain authoritative. Rotation, shear and mirroring do not use the atlas.
- Use direct page directories for one/two-byte CMap sources. Longer sources
  retain sparse directories. Scalar/CID queries avoid building string results.
- Resolve canonical collection CIDs only when drawn, reusing an already parsed
  font face. Explicit ToUnicode misses suppress canonical recovery. No full
  65,536-CID scan is needed at every substitute font load.
- Parse required sfnt/cmap/metric tables once for scalar queries. Variable
  metrics still use ttf-parser's fvar/avar/HVAR/gvar machinery with the required
  outline tables; GID-only queries do not compute metrics.
- Reuse bounded original-glyph checks by Weak LoadedFont identity without
  retaining font programs. Existing page decisions take priority. Composite
  loading shares one FontDescriptor/CIDSystemInfo resolution, and repeated
  simple codes reuse mapping/advance work while positioning every glyph.

The original uniformly sparse directory was rejected: six legacy mapping
controls regressed 19.61–82.39%. The partial run retains all 19 completed cases
in `font-cmap-controls-rejected-partial.json` and is not accepted. The
replacement passed all six targeted legacy diagnostics, 65.55–72.54% faster
than the original controls. This subset is diagnostic, not full acceptance.

The current source passes all six GPU controls, including repeated Latin
-16.14% and mixed supplementary -11.56%. All 31 current CMap controls also
pass against their recorded correctness-equivalent baseline. Complete timings,
confidence intervals and source bindings are in
[PRECISION_MEASUREMENTS.md](PRECISION_MEASUREMENTS.md).

## Validation and reproducibility

The latest broad command reports 847 passing tests across 46 suites, including
native font/document/content/writer/CPU/SVG tests, actual GPU checks and
doctests. Some historical corpus tests return early when their private inputs
are absent; that reported count is not evidence of historical-corpus coverage.
Dedicated portable font gates require their checked-in inputs and do not skip.

The CMap oracle compares all 1,190,674 encoding mappings and 113,496 canonical
strings with source-derived digests. Eight generator tests pass and regeneration
matches checked-in bytes. Additional portable tests deliberately separate code,
Unicode, CID and GID, use nonuniform PDF/font widths, and exercise malformed
UTF-16, variation sequences, late registration, empty clips, nonidentity CFF
charsets, 2000-UPEM vmtx/VORG metrics, overlapping tables and sfnt checksums.

Clippy with warnings denied and the actual wasm32-unknown-unknown target build
pass. CPU/GPU checks use actual Apple M5/Metal hardware with MSAA x4. Timestamp
queries are unavailable; timings include render/readback wall time. Placement
and ink checks do not assert identical antialiasing between CPU and GPU.

Benchmark inventories: original fonts 46, provider 8, constructor/copy diagnostics
11, packs 14, coverage 8, page fallback 9, writer fallback 3, authoritative CMaps
31 and GPU 6: **136 cases total**. Original workload definitions and input hashes
are unchanged.
New CMap cases use source-backed corrected semantics, so the old incorrect
mapping behavior is not a valid correctness-equivalent reference for them.

Final comparisons use 21 alternating AB/BA pairs per case, ten Criterion
samples, 0.1-second warmup and at least 0.25-second measurement windows on both
sides. The host is Apple M5/macOS 26.6 arm64/Rust 1.98.1. Actual adapter identity
is recorded and verified for every GPU process. Paired runs explicitly use
default scheduling for both archived executables, retaining the original
snapshot scheduling metadata separately. Different protocols cannot be pooled.
An upper paired bootstrap 95% bound above +5% cannot pass. Inconclusive is not
a pass. If fixed extensions are needed, `font_pool.py` retains every prior
observation and rejects mismatched executables, protocols and inventories.

Evidence is in the ignored build-output directory:

- Current correctness: `font-writer-preflight-full-{tests,clippy,wasm}.log`.
- Unchanged tools/generators: `font-acceptance-tool-tests.log`.
- Preceding complete evidence: `evidence/adjacent-repeat-pairs.json` and the
  matching table in `history/adjacent-repeat-measurements.md`.
- Current binary bindings: `font-writer-preflight-binary-bindings.json`.
- Current changed binaries: `font-writer-preflight-<suite>-{capture,paired}.json`;
  consult STATUS.md for completion. Byte-identical binaries retain the earlier
  evidence only with explicit source-to-executable bindings.

## Costs and remaining limits

Runtime mappings use 2,414,312 data bytes and an 18,550-byte text pool. They are
shared static sections, without per-font copies or runtime decompression. The
direct directories add 14,388 bytes over the rejected sparse layout, and the
four-byte legacy bucket adds 1,028 bytes. The
original font benchmark executable grew from 6,796,368 bytes before CMap work
to 9,315,344 bytes for the adjacent-repeat candidate. These are native benchmark sizes, not WASM download sizes
or measured resident-memory usage.

The historical private PDF corpus is absent: 0/19. The real Noto Sans SC
fixture is a licensed six-codepoint subset, not a full-font performance proxy.
Its acquisition/offline evidence cannot establish full CJK-font download cost.

Writer fallback is horizontal scalar authoring without shaping or bidi.
Automatic substitution does not guess private-use meanings or replace an
arbitrary multi-scalar extraction string with its first character. Coverage
depends on the supplied fonts; unsupported text remains explicit. The runtime
remains pure Rust and introduces no C/C++ font or shaping dependency.
