# perf(font): optimize measured glyph work and validate the complete fallback pipeline

## Problem or use case

The GPU path extracts an outline before checking the atlas, and the CPU path repeats outline work per glyph instance. Prior experiments show that indiscriminate bitmap or Face caching can have marginal or negative returns. The complete fallback pipeline therefore needs measured optimization rather than a blanket cache.

## Proposed behavior

Measure cache hit opportunities, query the GPU atlas before outline extraction where valid, and retain bounded outline/resolution caches only when the measured workloads benefit. Key caches by actual font identity, face index, variations, glyph and applicable device parameters. Pin font resources referenced by live display lists.

## Acceptance criteria

- Existing and new font workloads are measured on the same host against pre-change controls.
- Rotation, shear, mirroring, vertical positioning, variation instances and multiple documents cannot alias cache entries.
- Missing-glyph recovery is verified visually and semantically, not only by timing.
- Native, WASM, CPU, GPU, SVG and authoring integrations have appropriate build/test coverage.
- Publish an English implementation/benchmark report mapping each child issue to actual evidence, including any unavailable historical corpus or hardware.
- Repeatable regressions above the agreed 5% gate are fixed before completion.

Depends on the CMap sub-issue.

## Current working-tree implementation

The GPU atlas is checked before outline extraction, retaining page-local font
identity, GID and independent device sizes. Rotation, shear and mirroring keep
the vector path. Canonical collection CIDs resolve lazily instead of scanning
the whole collection at font load.

Scalar queries parse required sfnt/cmap/metric tables once. Variable advances
still use ttf-parser's fvar/avar/HVAR/gvar machinery with required dependencies.
GID-only queries do not compute metrics. Five equivalence tests cover fixture
code points, TTC, variable instances and malformed/unsorted/duplicate records.

Original-glyph checks share the registry lock and use lazy bounded storage:
4,096 glyphs and 1,024 Weak LoadedFont identities, retaining no font program
bytes. Existing page decisions take priority. Character coverage remains
directly stored. Composite loading shares FontDescriptor/CIDSystemInfo
resolution and page font maps reserve for actual resources.

Adjacent equal source identities reuse validation within a text operation.
Simple-font repeats reuse GID and advance while every glyph receives its own
spacing, rise and transformed position. Tests compare repeated and separate Tj
operations and keep distinct variation-selector sources separate.

Earlier cache layouts, CMap directories and setup regressions remain archived.
Their passes do not transfer to this candidate. No runtime C/C++ font or
shaping dependency has been introduced.

## Archived adjacent-repeat evidence

The exact candidate reports 846 passes across 46 suites, including portable
pipeline gates and actual Apple M5/Metal checks. Warnings-denied clippy and
actual wasm32 compilation pass. Eight generator tests, host acquisition/cache
tests, four browser-helper tests and 20 measurement-tool tests also pass.
Historical private PDFs remain absent (0/19).

All 136 first-run cases completed for one frozen source fingerprint:
a4368230a3d94b1a50f2b7ff3f2f4060dcbc77bf44528feac35786bf436f7be8.

| Suite | Pass | Inconclusive | Regression |
| --- | ---: | ---: | ---: |
| Provider | 8 | 0 | 0 |
| Packs | 14 | 0 | 0 |
| Coverage | 8 | 0 | 0 |
| Page fallback | 9 | 0 | 0 |
| CMaps | 30 | 1 | 0 |
| Original controls | 44 | 2 | 0 |
| GPU | 6 | 0 | 0 |
| Writer fallback | 2 | 1 | 0 |
| Constructor/copy diagnostics | 7 | 4 | 0 |
| Total | 128 | 8 | 0 |

Each case uses 21 alternating AB/BA pairs, ten samples, 0.1 s warmup and at
least 0.25 s measurement under default scheduling on both archived binaries.
The upper paired bootstrap 95% bound must stay at or below +5%.

Repeated-A provider setup improves 64–72%, static scalar queries about 81%,
Identity collection substitution 74%, GPU repeated Latin 10% and mixed
supplementary GPU rendering 8%. Inputs and glyph/position assertions are
unchanged. These are portable workloads, not historical-corpus claims.

All eight inconclusive cases received one fixed 42-pair extension, pooling
every original observation. The pooled result is 129 pass, six inconclusive
and one classified copy_heap regression (+28.64%, 95% +7.38..+45.78%). Four are production paths: vertical JIS embedded
rendering, TrueType construction, unique-CJK extraction and writer repeated
unsupported text. Four are constructor/copy diagnostics. Uncertainty is not
a pass, so full campaign acceptance remains open.

The pure-copy loop is identical in the original and current diagnostic
binaries: 93 AArch64 instructions after resolving relocated external calls,
with allocation/memcpy/deallocation and no font call. Earlier same-binary
calibration was itself inconclusive, and raw samples vary within a process.
This narrows the investigation but does not clear uncertain timings. Any
longer-window calibration uses a separate protocol and cannot be pooled with
the short-window data.

Current reports: font-repeated-original-provider-paired.json and
font-repeat-final-<suite>-paired.json. Fixed confirmations and pooled
reports retain those prefixes. Source/input/executable fingerprints and every
raw observation remain archived. English report, acceptance audit and issue
sources are under docs/performance/font-fallback/. The campaign is committed
and published as a pull request against `main`; every sub-issue stays open.

## Current writer preflight candidate

Coverage is now resolved for the entire authoring request before metrics or
embedding. Unsupported suffixes avoid all prefix metric parsing. Layout or
embedding errors still roll back newly admitted faces. A regression test
checks overflow after adding a face, existing content and later valid reuse.

All three writer workloads pass: repeated unsupported text -50.27%, mixed
authoring +2.45% (upper +3.02%) and single-face authoring +2.01% (upper +2.79%).
Report: font-writer-preflight-writer-paired.json.

Current source fingerprint:
11f20688d746c1f2ed3883588023be862d4821293372e8c3e02fc0ee5f84aa67.
All nine captures are complete. Exact executable hashes prove packs, coverage,
page fallback and diagnostics are byte-identical to their preceding captures,
with equal inputs/environment/inventories. All earlier outcomes, including
diagnostic failures, remain applicable through the explicit binding manifest.
Original controls, provider, CMaps and GPU changed and have completed fresh
comparisons, with fixed confirmations retaining every observation where needed.
All 125 functional controls pass. Across the full 136-case inventory, 135 pass
and one pure-copy diagnostic remains a classified regression after fixed
precision comparisons. No case is inconclusive. The single macOS regression is
documented as a platform-specific limitation; no pass is inferred merely from
unchanged benchmark source.

Full current validation reports 847 passes across 46 suites, warnings-denied
clippy and actual wasm32 compilation. The copy investigation continued:
identical-binary calibration was inconclusive with longer windows (+1.37%,
95% -7.40..+16.96%) and with background scheduling (-3.42%, -12.43..+3.46%).
Neither establishes a stable calibration, and protocols are never pooled.

Original controls pass all 46 after fixed confirmations: TrueType construction
-0.27% (upper +3.17%) and mixed-supplementary writer +2.99% (upper +3.41%).
Provider 8/8, CMaps 31/31 and GPU 6/6 pass fresh comparisons. GPU repeated Latin
improves 16.14%, mixed supplementary 11.56%. Packs 14/14, coverage 8/8 and page
fallback 9/9 remain proven through byte-identical executable bindings.

A standalone Rust probe separates allocation/free (about 18–19 ns, 5% batch
variation) from preallocated copying (231–622 ns, 35–37% batch variation).
It calls no zpdf code and reuses buffers. Inherited user-interactive QoS and
explicit user-initiated QoS both reproduce variation. This narrows the issue
toward copying/system behavior but does not establish a cause or clear any
diagnostic verdict. Source, hashes and all observations are preserved.

Current complete evidence: docs/performance/font-fallback/PRECISION_MEASUREMENTS.md and
evidence/writer-preflight-precision-pairs.json (347,438 bytes), plus
copy-diagnostic-investigation.md and evidence/copy-probe.json.
Binding manifest: font-writer-preflight-binary-bindings.json.
Current captures/reports: font-writer-preflight-<suite>-{capture,paired}.json.
Compact preceding evidence: docs/performance/font-fallback/evidence/adjacent-repeat-pairs.json
and history/adjacent-repeat-measurements.md. STATUS.md and ACCEPTANCE.md retain
the latest gate results and scope limits.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Precision follow-up

The fixed 512-pair current-binary copy calibration now passes: +1.04%, with
a 95% interval of -0.49..+3.17%, inside the two-sided 5% band. It uses the original
default-scheduler, ten-sample, 0.1 s warmup and 0.25 s measurement protocol.
This is a calibration subset, not acceptance of the before/after comparison.

The earlier log-ratio variation suggested about 318–319 pairs for a 5%
half-width under a simple planning approximation. The actual result still uses
the unchanged bootstrap decision rule. A separate nine-offset preallocated
copy matrix did not identify a consistently stable buffer layout; all 864
samples and its source are preserved.

A preregistered comparison now adds 1,024 pairs for construct_owned and 512
each for copy_heap, copy_aligned and copy_unaligned. These fixed counts use the
observed per-case variation; there is no early stopping or optional extension.
All 63 earlier observations per case are retained, and calibration data are
kept separate. The four original unresolved outcomes remain until complete
retained-data comparisons justify a new decision.

Plan: docs/performance/font-fallback/diagnostic-calibration-plan.md.
Calibration: font-copy-precision-calibration.json.
Comparison outputs: font-diagnostics-precision-<case>.json, followed by
font-diagnostics-precision-pooled.json. Library source and all 125
passing functional controls remain unchanged.

## Completed precision results and current unresolved case

All planned measurements completed and all earlier samples remain pooled.
Construct_owned passes with 1,087 pairs: -0.78%, 95% -1.44..-0.10%.
Copy_aligned passes with 575 pairs: +0.15%, -0.80..+1.41%.
Copy_unaligned passes with 575 pairs: -0.11%, -2.41..+1.10%.
Copy_heap remains a regression with 575 pairs: +16.86%, +10.40..+26.20%.
The complete diagnostic pool has ten passes and one regression.

A second 512-pair calibration uses distinct, equal-length archive paths with
byte-identical executables. It passes at -0.58%, -3.07..+0.13%; file paths alone
do not explain the remaining difference. Both normal same-binary and file-
identity calibrations are retained separately from actual comparisons.

Read-only inspection finds the same pre-copy stack frame, allocation size
and alignment, with no direct font call in that initialization prefix.
System memcpy uses a 16-KiB instruction-path threshold, but an independent
size/chunking probe does not show a sharp boundary slowdown; 8-KiB chunking
is slightly slower for this fixture. No library or benchmark input was changed
to force acceptance. The remaining cause is unresolved and the full gate stays
open, despite all 125 functional font controls and all construction controls
passing. No benchmark process is left running.

## Allocator bucketing and controlled single-allocation evidence

LLDB inspection records source and destination requests of 21,088 bytes, both
with usable size 24,576 and DefaultMallocZone. The installed libsystem_malloc
UUID is 971CDCFE-4D20-300E-A1D6-EC8672FDD258. Its instructions corroborate the
relevant operations in pinned Apple libmalloc revision
c49dafa25f1efe8607701ae6014a663ad2ee437f: caller-derived type hashes and keyed
pointer-bucket selection. This is not a claim that the entire public source
revision matches the installed OS.

Six alternating model inspections per binary predict all 24 actual xzone
indices correctly. Key fingerprints are stable within each executable and
differ between them. The source/destination type hashes differ by 1,498 in
the observed ASLR range. With these keys, the before binary permits bucket
steps 3 or 0 modulo four, while the current permits only 2 or 3. Only the
before binary can colocate the two allocation sites in one xzone. Public
source documents key derivation from the boot session and executable CDHash;
the two archived CDHashes differ.

The fixed process-local intervention retains 96 launches and 960 samples.
Separate versus same bucket rules increase copy cost by 95.02% before and
101.51% after. Because that experiment changes later allocation choices too,
a second fixed experiment changes only the source allocation's descriptor,
preserving original keys and every later allocation descriptor. It removes
all breakpoints before warmup and timing, while LLDB remains attached.

| Single-allocation diagnostic contrast | Change | Paired 95% interval |
| --- | ---: | --- |
| Before: separate vs same source bucket | +93.71% | +66.17..+115.05% |
| Current: separate vs same source bucket | +71.10% | +57.35..+87.06% |
| Current vs before: untouched | +19.45% | -2.10..+65.10% |
| Current vs before: same source bucket | +7.14% | -3.71..+20.99% |
| Current vs before: separate source bucket | -0.91% | -13.39..+6.64% |

All 96 successful launches and 960 samples are retained. One additional
attempt failed before warmup because zero is a special fallback-entropy type
descriptor. The candidate search now excludes zero and resumed with all 59
completed launches intact. One earlier successful zero-descriptor launch is
explicitly retained because its actual source xzone matched the requested
condition. Both script versions and the failed log are archived.

Changing only one allocation's bucket changes copy cost in both executables.
This identifies an allocator-mediated contributor, not exact attribution of
the entire original +16.86% or equivalence within 5%. The earlier same-arena
distance probe retains all 1,536 observations and does not reproduce the gap
by virtual distance alone. No cache/hardware cause is asserted.

Original source fingerprint, executable bytes, workloads and acceptance
results are unchanged: 135 pass, one classified regression, full gate open.
These diagnostic protocols are never pooled with the original 575 pairs.
There is no proposed shipping allocator override. All experiments completed.

Reports: docs/performance/font-fallback/copy-diagnostic-investigation.md and
diagnostic-calibration-plan.md. Evidence: copy-allocator-model.json,
copy-allocator-intervention.json, copy-source-type-intervention.json and
copy-source-type-provenance.json under the report's evidence/ directory.

## Independent Linux recheck

The user supplied a remote development container. Its clean checkout at
77732617a558e6d85c375cc6624d4d1c49ebe648 is preserved; separate control/current
repositories live under its ignored target directory. Current source matches
11f20688d746c1f2ed3883588023be862d4821293372e8c3e02fc0ee5f84aa67. The new
control uses production 137ab2b plus matching benchmark workloads and fixtures.
Its workspace-only lock adjustment leaves all 873 third-party package blocks
byte-identical. It is not a reconstruction of the intermediate macOS source.

Environment: Linux x86_64, Intel Xeon Gold 6133, 40 visible logical CPUs,
Microsoft hypervisor, glibc 2.35, Rust 1.96.0/LLVM 22.1.2. No CPU or memory
cgroup maximum is reported. Four bounded compilation jobs share a task-owned
cache. Compilation and validation finish before timing. No hardware GPU device
is exposed; the old remote corpus has only 1/19 manifest entries present and
hash-matching, and that file is not counted as exercised by the isolated run.

Native release validation reports 788 passes across 42 suites. All 57 common
first comparisons completed with 21 AB/BA pairs: 52 pass, five inconclusive,
no regressions. Fixed extensions add 256 constructor, 64 aligned-copy, 64
static-copy, 128 destruction and 256 mixed-extraction pairs, retaining all
earlier data. The resulting independent Linux result is 56 pass, one
inconclusive, no classified regressions:

- Same-binary copy calibration: -0.12%, 95% -1.18..+1.34%, 21 pairs; pass.
- Copy_heap: -0.13%, -1.31..+1.01%, 21 pairs; pass.
- All 46 original font controls pass; mixed-supplementary extraction is
  +4.02%, +3.70..+4.53%, over 277 retained pairs.
- Ten of eleven diagnostics pass. Copy_aligned remains inconclusive at
  +4.52%, +2.45..+6.42%, over 85 retained pairs.

A separately preregistered aligned-copy stage added exactly 1,024 pairs.
Its count follows the retained log-ratio variance and a 0.5% planning target
half-width. The unchanged bootstrap decision still determines the outcome;
there was no guaranteed pass, early stop or result-dependent extension. All
1,109 aligned-copy observations are retained. The final result passes at
**+3.30%, 95% +2.89..+3.68%**. All eleven diagnostics and all 46 original font
controls now pass: **57/57 Linux cases, no inconclusive cases or regressions**.
Original source and thresholds remain frozen. The earlier 56-pass/one-
inconclusive result above is retained as the completed preceding stage.

Reports and raw observations are retained under the isolated remote task
directory and mirrored locally in the archived font-linux-recheck-20261009 evidence directory.
The first-run export is docs/performance/font-fallback/evidence/linux-recheck-first-pairs.json;
the final export is evidence/linux-recheck-final-pairs.json, with all 57 rows
in LINUX_MEASUREMENTS.md. The plan and results are in LINUX_RECHECK.md.
The post-run audit verifies both source fingerprints, all four executable
hashes, the unchanged original checkout and 3,010 pairs including calibration:
6,020 raw sample files / 60,200 individual samples. Raw observations and
binaries are mirrored locally in linux-evidence.tar.gz (18,735,835 bytes,
SHA-256 82937e19b492a2dcd268179fa6fd7c7d1f8234f70d4bdab0c850af196e909027).
That font-only comparison is complete. Linux results do not
erase or relabel the recorded macOS copy_heap regression. Full acceptance and
all issues remain open.

## Original pipeline coverage follow-up

The independent 57-case Linux font comparison is complete and unchanged.
Completion audit found a separate missing requirement: timing the existing
synthetic stages controls. The deterministic perf_gate cannot establish that
relative timing result. The original target covers 135 workloads across nine
fixtures, three DPIs and five stages, including Type3 text and PNG encoding.

Its common helper also ignored explicit CLI budgets. That benchmark-only bug
is fixed identically in the control/current trees and verified with focused
tests, Clippy, a live configuration probe and all-case smoke checks. Production
library code is unchanged; nine Mac and four Linux font binaries are exactly
byte-identical to their prior captures. Reuse includes unsuccessful outcomes.

A fixed Type3 calibration and all 21 full-inventory pairs have completed:
**131 pass, four inconclusive, zero classified regressions** under the original
+5% upper-confidence-bound gate. Every parse, interpret, encode and end-to-end
case passes. Four CPU-render cases remain uncertain: curves @300 (-0.24%,
95% -15.48..+19.97%), rect_fills @300 (-16.79%, -23.48..+11.26%), strokes @300
(-2.92%, -10.60..+14.12%) and Type3 @150 (+1.24%, -3.18..+5.62%).

An independent local audit verifies all 57,120 raw samples, 84 complete
processes, AB/BA order, actual budgets, source/input/executable hashes and the
unchanged old checkout. All data and binaries are archived locally and remotely.
The archive is 29,258,850 bytes, SHA-256
75ed6c620cde0ec578a5f976e8761ffe959b4035b2a273908a0cc5da810052a8.
See PIPELINE_RECHECK.md, PIPELINE_MEASUREMENTS.md and evidence/pipeline-pairs.json
under docs/performance/font-fallback/.

Sampling inspection associates the uncertain cases with batches retaining
multiple RGBA outputs and, for Type3, different iteration plans. A separate
fixed 21-block same-binary diagnostic now checks SmallInput versus PerIteration
repeatability outside the frozen source trees. This is not acceptance evidence
and is never pooled with the whole-suite observations. The original matrix
remains 131 pass and four inconclusive; full campaign acceptance remains open.

The output-retention diagnostic now completes all 21 blocks. SmallInput has
three passing calibrations and one inconclusive case: curves @300 has 95%
-5.03..+6.90% between two runs of the same executable. PerIteration passes all
four calibrations: curves -1.03..+0.74%, rect_fills -1.87..+0.80%, strokes
-1.51..+1.34%, Type3 +0.03..+1.77%. All 84 processes / 3,360 raw samples are
independently verified. Evidence: evidence/pipeline-batching.json.

The mode comparison changes benchmark cost, not production font behavior.
Its narrower context cannot clear the original whole-suite uncertainty.
Investigating a one-output-at-a-time benchmark is justified, but any repair
requires fresh control/current comparisons with explicit workload identities.
All frozen production source and original executable hashes remain unchanged.
The diagnostic archive is mirrored locally (4,645,333 bytes, SHA-256
e77f2215a084bb628ced7482d353eeda6a7de6148edb4632329a630150ab8ba7).
No benchmark process remains active at this checkpoint; acceptance stays open.

The supported benchmark repair now changes only stages' output retention to
PerIteration, identically in both source trees. All 135 cases and renderer
operations are preserved; its distinct protocol requires new captures,
four-case same-binary calibration (21 pairs) and a fresh complete comparison
(21 full-suite pairs), never pooling the original SmallInput data. Local
formatting and Clippy pass, and all nine Mac font executables remain unchanged.
Remote validation/capture subsequently completed. Missing Rust 1.96 Clippy was
found before measurement; its failed preparation is retained, and the component
was installed without changing the default toolchain.

Both repaired Linux captures are complete: Clippy, seven correctness tests and
135-case smoke checks pass, all 14 inputs match, and four Linux font binaries
remain unchanged alongside the nine Mac binaries. Formal 21-pair calibration
passes all four cases: curves 95% -1.09..+0.83%, rect_fills -1.11..+1.10%,
strokes -0.14..+1.51%, Type3 -1.25..+1.12%. The subsequent complete 21-pair
comparison is reported below. Exact identities:
evidence/benchmark-bounded-bindings.json.

The repaired full-inventory comparison is now complete: **135/135 pass**, no
inconclusive cases, no classified regressions, largest upper 95% bound +4.11%.
The four previously uncertain workloads pass in this separate protocol:
curves @300 +0.36% (95% -1.13..+1.33%), rect_fills @300 +0.13%
(-0.74..+1.36%), strokes @300 -0.03% (-1.01..+0.97%), Type3 @150 +0.33%
(-0.41..+1.06%). This compares production control/current with equal bounded
output lifetimes; changing the harness is not counted as a font speedup.

Independent audit verifies all 58,380 raw samples, 84 complete processes,
AB/BA order, actual budgets and exact source/input/executable identities.
The original old checkout remains clean. The 25,185,198-byte archive is mirrored
on both hosts, SHA-256
bf6e1b97aa7f3db0bedd37993aae439567c35db82d84d5b68968645e65bc6391.
Evidence: PIPELINE_BOUNDED_MEASUREMENTS.md, evidence/pipeline-bounded-pairs.json,
and evidence/pipeline-bounded-audit.json under docs/performance/font-fallback/.
The original SmallInput run and separate diagnostic remain intact, never pooled
with this result. No pipeline measurement process remains active. Full campaign
acceptance remains open for the retained macOS heap-copy diagnostic.

The fixed allocation-callsite follow-up has completed and is independently
audited. Both probe builds have identical copy helper instructions, different
CodeDirectory identities, and verified source/destination allocation callsites.
All 168 processes / 1,680 raw samples are retained. Same-binary calibrations
remain inconclusive: A/shared 95% -2.92..+18.39%, A/distinct -6.15..+26.92%,
B/shared -10.12..+6.01%, B/distinct -13.51..+10.01%.

Unifying allocation callsites is insufficient to establish repeatability in
this protocol, so that benchmark change is not adopted. No production code,
original diagnostic or threshold changes. The original 575-pair +16.86%
copy_heap regression remains recorded. Evidence: copy-callsite-results.md and
evidence/copy-callsite.json under docs/performance/font-fallback/. Full local
archive: 2,479,731 bytes, SHA-256
491f1fc7853d8f3cb12dcccd68fd2f3d8650a31a4b1af5dbcabdd03833dc9382.
No benchmark process remains active; full-platform acceptance remains open.

A separate fixed precision stage is now complete under the original full-platform
scope. Width-based planning selected 512 total blocks, retaining all 21 first-run
blocks and adding 491 with the same four conditions, binaries, exact paths,
warmup, iterations, sample count and scheduler. Current boot time predates the
first run; boot identity is checked throughout. The complete planned inventory
contains 4,096 processes / 40,960 raw samples, including all first-run data.

The +/-5% calibration gate is unchanged. No intermediate stopping or later
result-dependent extension occurred. The audit recomputes every raw median and
verdict, verifies order and completion markers, and proves the initial 168
processes are unchanged. All four same-binary calibrations pass. Shared and
distinct modes both pass, and their probe-binary identity contrasts are near
zero; this does not establish a unique benefit from unifying callsites.

The probe cannot clear the original 575-pair copy_heap regression or substitute
for a matched zpdf comparison. No production or original benchmark change is
adopted. Evidence: copy-callsite-precision-results.md and
evidence/copy-callsite-precision.json. Archive: 4,446,396 bytes, SHA-256
ae1e2718889d3792af6d9c76b347da3694ae050a1720393d58e7390003f67aa8.
All planned measurements are complete. The user selected the Linux environment
as the final performance acceptance platform. Under that scope the 57-case
Linux font inventory and the repaired 135-case Linux pipeline inventory both
pass the unchanged 5% gate with every raw sample independently recomputed. The
original 575-pair macOS copy_heap result is retained as a documented
platform-specific limitation and is not excluded, relabeled or cleared.
