# bench(font): add reproducible coverage, loading and rendering baselines

## Problem or use case

The four current Criterion targets cover pipeline stages and throughput, but the portable synthetic corpus exercises Type3 text only. There is no direct benchmark for TrueType/CFF loading, simple/composite mapping, repeated versus unique glyphs, missing glyphs or font fallback. The existing Windows baseline cannot establish regressions on a different host.

## Proposed behavior

Add a dedicated fonts target with deterministic synthetic Latin/CJK/supplementary-plane fixtures, embedded-font and missing-font cases, repeated/unique text, cold construction and warm document reuse, extraction and CPU rendering controls. Include variable-font and CFF coverage. Fixtures must assert actual glyph/ink/text behavior before timing.

Add strict, scoped baseline recording/comparison with fixture and environment identity, expected case coverage, and no silent success for empty, stale or incomplete results. Preserve existing benchmark workflows. Document how later provider, pack, fallback and CMap cases extend this suite.

## Acceptance criteria

- A clean clone can run the font suite without installed fonts, private PDFs or network access.
- Cold and warm measurements measure the stated operation; setup excluded from a stage is documented.
- Correctness checks prevent dropped glyphs from appearing as speedups.
- A same-host release baseline and an unchanged rerun are recorded before production font behavior changes.
- CI checks the font fixture semantics and benchmark harness.
- Missing original private corpus files remain explicitly reported, never represented as a full historical-corpus comparison.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

The dedicated inventory contains 136 cases: original fonts 46, provider 8,
packs 14, coverage 8, page fallback 9, writer fallback 3, CMaps 31, GPU 6 and
constructor/copy diagnostics 11. Workload identities and original input hashes
are retained. Portable checks verify actual glyph IDs, PDF placement,
extraction and ink before timing.

Snapshots record host/toolchain, source and executable hashes, input hashes,
settings and exact case inventories. Paired comparisons alternate AB/BA with
21 pairs, ten samples, 0.1 s warmup and at least 0.25 s measurement. The current
run explicitly uses default scheduling on both sides; snapshot scheduling
metadata is retained separately. The upper paired bootstrap 95% bound must be
at most +5%. Inconclusive results are not passes.

Atomic pair checkpoints and per-process completion markers support interrupted
runs without discarding validated observations. Fixed extensions retain all
earlier observations and reject mismatched protocols. Twenty measurement-tool
tests pass. CI runs portable fixture/source-oracle/cache-bound tests and the
existing synthetic pipeline gate.

The exact current source has 847 reported test passes across 46 suites,
warnings-denied clippy and actual wasm32 compilation. Historical private PDFs
remain absent (0/19); skipped corpus paths are not counted as historical
coverage. Current complete comparisons and the copy/allocator investigation
remain tracked in #52. The campaign is committed and published as a pull
request against `main`; every sub-issue stays open.

The current 136-case inventory is complete: all 125 functional controls pass;
including diagnostics, 135 pass and one copy diagnostic remains classified as
a regression after fixed precision comparisons. No case is inconclusive. Current observations, exact executable bindings
and calibration metadata are exported in PRECISION_MEASUREMENTS.md and
evidence/writer-preflight-precision-pairs.json under docs/performance/font-fallback/.
No unsuccessful outcome has been removed or relabeled.

## Original pipeline audit and explicit budget repair

The completion audit found that the requested original synthetic pipeline
timing control had not been established by perf_gate: that test checks exact
composition and a loose wall-clock ceiling. The existing stages target covers
135 workloads (nine synthetic fixtures, three DPIs, five stages including PNG
encoding). All original operations and fixture bytes are retained.

A configuration probe exposed a harness bug: requesting ten samples, 0.1 s
warmup and 0.25 s measurement still ran 100 samples, one-second warmup and
three-second measurement. The common helper's group-level settings overrode
Criterion's explicit global CLI values. It now leaves those group overrides
unset when the corresponding CLI option is present. Default budgets and
cost-adaptive sampling remain unchanged. Four focused tests, warnings-denied
Clippy and formatting checks pass; CI includes the new tests. The actual
repaired probe confirms ten samples and the requested windows.

Both Linux control/current trees pass seven budget/perf-gate tests and all
135 benchmark cases in test mode. Fourteen workload input hashes match. The
same helper repair is applied to both trees, with no production library edits.
Exact executable hashes bind all nine earlier Mac font captures and four Linux
font captures to the new whole-source identity; all prior outcomes remain.

The fixed Type3 same-binary calibration and all 21 full-inventory pairs are
complete: **131 pass, four inconclusive, zero classified regressions**. Every
parse, interpret, encode and end-to-end case passes (27 each); CPU rendering
has 23 passes and four uncertain cases. The complete matrix is not accepted.
The original +5% upper-confidence-bound gate remains unchanged.

The independent local audit recomputes all medians, ratios and verdicts from
57,120 raw samples, verifies 84 complete process markers and exact input/binary
hashes, and confirms AB/BA order and actual CLI budgets. The 29,258,850-byte
raw archive is mirrored locally, SHA-256
75ed6c620cde0ec578a5f976e8761ffe959b4035b2a273908a0cc5da810052a8.
Reports: docs/performance/font-fallback/PIPELINE_RECHECK.md,
PIPELINE_MEASUREMENTS.md and evidence/pipeline-pairs.json.

Raw data associate the uncertain renderer measurements with Criterion's
batching of retained RGBA outputs; Type3 also has two different iteration
plans. A separately preregistered same-binary diagnostic compares SmallInput
and PerIteration in 21 fixed blocks. It is outside the frozen source trees,
does not change production code, and cannot relabel or pool with the original
135-case result. All original observations and macOS outcomes remain retained.

That diagnostic is complete: SmallInput passes three same-binary calibrations
and has one inconclusive result (curves @300, 95% -5.03..+6.90%). PerIteration
passes all four, with upper bounds at most +1.77%. All 84 processes and 3,360
raw samples are retained and independently verified. This is evidence for
investigating the benchmark's output lifecycle, not a passing production
comparison. Any repair requires fresh control/current measurements; the
original matrix remains 131 pass and four inconclusive.

The original stages renderer benchmark now uses PerIteration on both sides,
documented as a distinct protocol. Setup and output destruction stay outside
timing; one RGBA output is live per iteration. Fixtures and production code
are unchanged. Fresh capture, Clippy and all-case smoke checks precede 21 fixed
four-case calibration pairs; only a passing calibration starts the 21-pair
full-inventory comparison. The original data and conclusions remain archived.

Both sides passed all preparation checks, with 14 matched workload inputs and
unchanged prior font binaries. All four fixed 21-pair calibrations passed; the
largest upper bound was +1.51%. The subsequent complete 135-case comparison
remained separate from the original protocol and is reported below.

The fixed new comparison has completed: **all 135 cases pass**, with no
inconclusive cases or classified regressions; largest upper 95% bound +4.11%.
All 58,380 raw samples and 84 process markers were independently verified,
alongside source/input/executable identities, execution order and actual budgets.
The full archive is retained locally and remotely (25,185,198 bytes, SHA-256
bf6e1b97aa7f3db0bedd37993aae439567c35db82d84d5b68968645e65bc6391).
Current table: docs/performance/font-fallback/PIPELINE_BOUNDED_MEASUREMENTS.md.
All original SmallInput and separate diagnostic outcomes remain recorded;
no cross-protocol pooling is used. Pipeline timing coverage is complete under
the repaired protocol. Full campaign acceptance remains open for macOS copy_heap.

An independent fixed shared-allocation-callsite probe retained 168 processes
and 1,680 raw samples, but all four same-binary calibrations are inconclusive.
Its two builds have identical copy helper instructions and distinct signing
identities. This evidence is insufficient to adopt the proposed benchmark
control; original diagnostics and their outcomes stay unchanged. Details:
docs/performance/font-fallback/copy-callsite-results.md.

A fixed precision extension distinguishes inadequate paired precision from a
usable measurement control. It keeps the complete four-condition inventory,
unchanged binaries/settings and every original observation, with 512 total
blocks selected from the measured interval widths. The independent audit
verifies 4,096 processes / 40,960 samples and all four calibrations pass.
Both shared and distinct modes pass, so no specific benchmark change is adopted.
This remains separate from the original copy_heap benchmark and cannot clear
its old/new result. See copy-callsite-precision-results.md. All planned timing
runs are complete. The user selected the Linux environment as the final
performance acceptance platform; both Linux inventories pass the unchanged 5%
gate, and the retained macOS copy_heap diagnostic is documented as a
platform-specific limitation.
