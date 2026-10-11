# Improve font coverage and fallback with benchmark-gated implementation

## Problem or use case

zpdf reads embedded fonts and substitutes installed fonts, but substitution selects one face by name without a complete per-glyph coverage fallback chain. The renderer has no application font provider, browser deployments cannot rely on system fonts, and predefined CJK CMaps have incomplete mappings. Large generated encoding tables map character codes to Unicode; they do not contain glyph outlines and cannot be replaced by downloading a TTF.

The current benchmark suite measures pipeline stages but lacks dedicated font loading, mapping, fallback and font-pack workloads. Its checked-in baseline is machine-specific and incomplete.

## Proposed behavior

Implement the following dependent work packages in order, starting with reproducible measurement:
1. Dedicated font benchmarks, deterministic fixtures and strict comparison gates.
2. An additive font-provider API for registered in-memory and system fonts.
3. Bounded, lazily decoded font packs and optional host-managed acquisition.
4. Coverage-aware glyph fallback with positive/negative caching and preserved PDF placement.
5. Complete supported CJK encoding/CID mappings and explicit unsupported cases.
6. Benchmark-driven glyph/cache improvements and integration validation.

Preserve usable embedded glyphs. Resolve missing glyphs through reliable Unicode or collection mappings, never through arbitrary GID reuse. Keep network requests and decompression outside per-glyph rendering. Support native and WASM callers without introducing C/C++ dependencies.

## Alternatives considered

Keeping only system-font substitution leaves headless and browser environments dependent on unavailable fonts. Replacing encoding tables with TTF files does not solve code-to-Unicode or code-to-CID mapping. Unconditionally caching all glyph bitmaps is not justified by the prior performance experiments.

## Compatibility and specification impact

Prefer additive public APIs and preserve the default path for callers that do not register extra fonts. PDF character codes, CIDs, Unicode and GIDs are distinct namespaces (ISO 32000-1 sections 9.6-9.10). Preserve PDF widths, vertical metrics, explicit positioning and text extraction.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Execution

Each child issue will contain scope-specific acceptance criteria and benchmark evidence. All source comments, issue text, commit messages and PR descriptions produced for this work will use English. This technical plan is AI-assisted and was requested explicitly by the repository user.

## Linked sub-issues

All six packages have working-tree implementations. Full performance acceptance
remains open in #52. The preceding adjacent-repeat candidate completed all
136 cases and fixed confirmations: 129 pass, six inconclusive and one classified
pure-copy diagnostic regression. Every paired observation is retained.

The current writer preflight change avoids computing prefix metrics when an
unsupported suffix will fail. All three writer controls pass: the unsupported
path is 50.27% faster, mixed authoring +2.45% and single-face authoring +2.01%.
All 125 functional font performance controls now pass. Across the complete
136-case inventory, 135 pass and only the pure-copy diagnostic remains a
classified regression after retained-data precision comparisons. Full acceptance is still open. Exact executable
hashes preserve the existing pack/coverage/page-fallback passes and unresolved
diagnostics; changed binaries receive fresh measurements.

Current validation reports 847 passes across 46 suites, warnings-denied clippy
and actual wasm32 compilation. Historical private PDFs remain absent (0/19);
the real Noto fixture has only six code points. The campaign is committed and
published as pull request #53 (https://github.com/Xero-Team/zpdf/pull/53);
every sub-issue stays open. Evidence, implementation reports and limits are
under docs/performance/font-fallback/.

- [#47: bench(font): add reproducible coverage, loading and rendering baselines](https://github.com/Xero-Team/zpdf/issues/47)
- [#48: feat(font): support registered memory fonts through a shared provider](https://github.com/Xero-Team/zpdf/issues/48)
- [#49: feat(font): load bounded fallback font packs lazily](https://github.com/Xero-Team/zpdf/issues/49)
- [#50: fix(font): resolve missing glyphs by coverage while preserving PDF placement](https://github.com/Xero-Team/zpdf/issues/50)
- [#51: fix(font): complete supported CJK CMaps and encoding coverage](https://github.com/Xero-Team/zpdf/issues/51)
- [#52: perf(font): optimize measured glyph work and validate the complete fallback pipeline](https://github.com/Xero-Team/zpdf/issues/52)

## Allocator investigation follow-up

Installed macOS allocator inspection now corroborates an executable-specific
type-bucketing difference in the unchanged pure-copy control. All 24 predicted
xzone indices match actual observations. Under the observed boot's keys, the
before binary can place source and destination in one bucket; the current
binary cannot at these allocation sites.

A fixed diagnostic changes only the source allocation's type descriptor,
leaving original keys and later allocation descriptors intact. Separate versus
same source buckets increases copy time by 93.71% before (95%
+66.17..+115.05%) and 71.10% after (+57.35..+87.06%). All 96 successful
launches, 960 samples and one pre-timing failed attempt are retained. A broader
earlier allocator intervention is retained separately too.

This establishes an allocator-mediated contributor, not a 5% equivalence
result or a complete explanation of the original +16.86%. No production code,
original workload or gate was changed. Full acceptance remains 135 pass and
one regression, with all issues open. Evidence and reproducible diagnostics:
`docs/performance/font-fallback/copy-diagnostic-investigation.md` and
`evidence/copy-source-type-intervention.json`.

## Independent Linux validation

The user supplied an x86_64 Linux development container for independent tests.
Its existing clean checkout is preserved. Isolated control/current trees use
production baseline 137ab2b and the frozen current implementation, with matched
workloads and all 873 third-party dependency records identical. This does not
reconstruct the intermediate macOS baseline or replace its observations.

Linux release native validation reports 788 passes across 42 suites. A fixed
21-pair same-binary copy calibration passes. The 57 common benchmark cases and
all fixed precision extensions have completed: **57/57 pass, no inconclusive
cases or classified regressions**. All 46 original font controls pass.
Mixed-supplementary extraction passes at +4.02% (upper +4.53%, 277 pairs).
Copy_heap passes at -0.13% (95% -1.31..+1.01%).

Copy_aligned passes at +3.30% (95% +2.89..+3.68%) after the preregistered
1,024-pair stage, retaining all 85 earlier pairs for 1,109 total. The full
common Linux inventory passes the original gate. The audit verifies unchanged
source, all four executable hashes and 60,200 raw samples including calibration.
A local mirror preserves the completed raw archive independently of the
container. That font-only run is complete. The original macOS diagnostic record
and full campaign scope remain explicit; no data is relabeled or discarded.

Evidence: docs/performance/font-fallback/LINUX_RECHECK.md, LINUX_MEASUREMENTS.md,
and evidence/linux-recheck-final-pairs.json. The first-run export is preserved.
The old remote checkout has 1/19
historical manifest files with matching hashes; that file was not included in
the isolated native test run. No full historical-corpus or Linux hardware-GPU
coverage is claimed. Production code was not changed during this recheck.

## Original synthetic pipeline comparison

The completion audit identified missing relative timing for the existing
135-case stages matrix. Its helper also overrode explicit Criterion CLI
budgets; this benchmark-only bug is repaired identically on both sides and
covered by focused tests and an actual configuration probe. All earlier font
binaries remain byte-identical, retaining both passes and failures.

The fixed calibration and all 21 full-suite process pairs are now complete:
**131 pass, four inconclusive, zero classified regressions**. All 27 end-to-end
cases pass (largest upper bound +2.67%). The four uncertain cases are CPU
rendering of curves/rect_fills/strokes @300 and Type3 @150, so the full matrix
is not accepted yet. The original 5% gate is unchanged.

An independent audit recomputes every result from all 57,120 raw samples and
verifies exact identities and execution order. The 29,258,850-byte evidence
archive is mirrored locally, SHA-256
75ed6c620cde0ec578a5f976e8761ffe959b4035b2a273908a0cc5da810052a8.
Detailed results: docs/performance/font-fallback/PIPELINE_RECHECK.md,
PIPELINE_MEASUREMENTS.md and evidence/pipeline-pairs.json.

A separately preregistered diagnostic checks whether Criterion output batching
contributes to the observed render variance. It uses one current binary and
21 fixed blocks, retains all data, and cannot replace or pool with the original
comparison. Frozen production sources and the macOS diagnostic record remain
unchanged. All issues remain open.

The output-retention diagnostic has now completed all 21 blocks. SmallInput
has three passing same-binary calibrations and one inconclusive case (curves
@300, 95% -5.03..+6.90%); PerIteration passes all four, with upper bounds at
most +1.77%. All 3,360 raw samples were independently verified and archived.
This supports investigating a bounded output lifecycle in the renderer bench,
but it is not a production before/after comparison. Original pipeline and
macOS outcomes remain unchanged; fresh evidence is required for any repair.
No benchmark process remains active at this checkpoint.

The next preregistered stage applies PerIteration identically to both original
stages targets, preserving renderer setup/render/output-drop boundaries and
all 135 case identities. Production libraries are unchanged. All nine Mac
font executables remain byte-identical under current source fingerprint
b078a4d72637d8707f4ed0ab466be5d307a07f89b6e13892e63b2a2a581821d9.
Fresh Linux captures and correctness checks subsequently completed. The fixed
protocol used 21 same-current-binary pairs for the four uncertain cases, then
21 full-inventory pairs after calibration passed. New and original protocols
are never pooled. Missing remote Clippy was detected before any timing; its
failed preparation log is retained and the component was installed.

Both repaired captures now pass Clippy, seven budget/perf-gate tests and all
135 smoke cases. All nine Mac and four Linux font executables remain unchanged.
The fixed four-case calibration completed 21 pairs and passed every case
(largest upper bound +1.51%). The subsequent 21-pair full-inventory comparison
is reported below; complete-campaign acceptance remains pending.

The repaired complete pipeline run is now finished and independently audited:
**135/135 pass, zero inconclusive, zero regression**, with a largest upper 95%
bound of +4.11%. Every case retains all 21 full-suite AB/BA pairs. The audit
recomputes 58,380 raw samples, checks 84 completion markers and exact identities,
and verifies the unchanged clean old checkout. Archive mirrored on both hosts:
25,185,198 bytes, SHA-256
bf6e1b97aa7f3db0bedd37993aae439567c35db82d84d5b68968645e65bc6391.
See PIPELINE_BOUNDED_MEASUREMENTS.md, evidence/pipeline-bounded-pairs.json and
evidence/pipeline-bounded-audit.json under docs/performance/font-fallback/.

Linux font controls (57) and the repaired pipeline (135) now pass their
separate complete protocols. The original SmallInput run, batching diagnostic
and macOS copy_heap regression remain intact and are never pooled or relabeled.
No pipeline benchmark remains active. Overall campaign acceptance stays open
for the historical macOS heap-copy diagnostic; all issues remain open.

A further fixed Mac allocation-callsite probe is complete: 168 launches,
1,680 raw samples, and all four same-binary calibrations inconclusive. Using
one allocation callsite for both source and destination did not establish
stable timing in this protocol, so this proposed control change is not adopted.
Both original diagnostic binaries and production source remain unchanged.
Evidence: docs/performance/font-fallback/copy-callsite-results.md and
evidence/copy-callsite.json. The existing full-platform gate remains open;
the historical regression is not excluded or relabeled.

Under the unchanged full-platform scope, a separately preregistered precision
stage retained all 21 callsite-probe blocks and added 491 for 512 total. The
count follows completed interval-width planning; all four conditions, exact
archived binaries, sample windows and same boot are retained. No early stopping
or threshold change occurred. The independently audited 4,096 processes and
40,960 samples pass all four same-binary calibrations. Both shared and distinct
modes pass, so the result does not establish a specific advantage for adopting
the proposed benchmark change. It is not an old/new zpdf comparison and cannot
replace production acceptance or clear the original regression. Results:
docs/performance/font-fallback/copy-callsite-precision-results.md.

All planned measurements are complete. Linux font and pipeline inventories
pass; all 125 functional Mac controls pass. The user selected the Linux
environment as the final performance acceptance platform. Under that scope the
57-case Linux font inventory and the repaired 135-case Linux pipeline inventory
both pass the unchanged 5% gate with every raw sample independently recomputed.
The original Mac pure-copy result, whose loop calls no font code, is retained as
a documented platform-specific limitation. No gate, fixture or case was
removed, no measurement was relabeled, and no issue was closed.
