# Original synthetic pipeline recheck

The completion audit found a scope gap: the font guide and issue #47 require
the existing pipeline benchmarks as controls. The completed deterministic
`perf_gate` tests verify composition and a loose ten-second ceiling; they do
not establish a relative timing result. The separate 57-case Linux font result
remains complete and retained.

## Configuration defect and repair

The original `stages` target lists 135 cases: nine checked-in synthetic PDFs,
three DPIs (96, 150, 300), and parse, interpret, CPU render, PNG encode and total.
The requested configuration probe used ten samples, 0.1 s warmup and 0.25 s
measurement. Its actual output used 100 samples, one-second warmup and about
three-second measurement. Criterion's per-group settings override its global
CLI configuration; the shared helper unconditionally set all three values.
The original probe is retained and excluded from acceptance measurements.

The helper now recognizes explicit CLI budget flags and leaves those group
overrides unset, inheriting Criterion's already parsed and validated values.
Defaults remain one-second warmup, three-second measurement and cost-adaptive
sample counts. Split and equals argument forms are supported; positional
filters after `--` are not interpreted as options. Four focused tests verify
selection, and the native probe verifies the actual sample count and windows.
The CI gate includes these tests. Timed operations and PDF fixtures do not
change. The same helper repair applies to both isolated comparison trees.

Production library code is unchanged. All earlier font executable hashes must
be checked explicitly before retaining their previous outcomes under the new
whole-source fingerprint, including the historical macOS diagnostic failure.

## Preregistered measurement protocol

Use the existing user-authorized Linux container and Rust 1.96.0/glibc 2.35.
Retain baseline production commit 137ab2b and the current implementation, with
matched third-party dependencies. Archive both newly compiled stages binaries,
source fingerprints and all workload hashes. Run `benchmark_budget` and
`perf_gate` on both sides before measurements. Assert complete 135-case
inventories and identical hashes for the nine fixtures, stages.rs and its
shared workload/helper sources. Set `ZPDF_BENCH_SYNTHETIC_ONLY=1` explicitly;
this is not historical-corpus or hardware GPU coverage.

First verify the fixed CLI configuration on the Type3 interpretation case.
Then run 21 fixed identical-current-binary calibration pairs for
`stages/interpret/text_type3-synthetic@150`, alternating AB/BA. Require its
entire paired bootstrap 95% interval within 0.95–1.05. This is a calibration
subset, not full pipeline acceptance.

After calibration passes, run 21 fixed full-inventory pairs. Each process
executes all 135 cases with ten samples, 0.1 s warmup and 0.25 s measurement.
Alternate control/current process order AB/BA per pair. This avoids paying
every untimed setup probe again for each separately launched case. Both sides
execute the unchanged case order and operations. The process grouping differs
from the earlier per-case font protocol, so reports remain separate.

Require exactly 135 estimates and ten raw samples per case in every completed
process, and matching benchmark identities. Keep every process log and raw
sample; reject partial or invalid runs. Recompute per-case median ratios and
paired bootstrap intervals using the existing decision function. The upper
95% bound must be at most +5%; uncertainty is not a pass. No early stopping or
result-dependent extension. Any later confirmation is planned separately
after the complete fixed run, retaining all earlier observations.

An atomic progress report and per-process completion markers must preserve
completed pairs if interrupted. Never mix these data with the 57-case font
comparison or the older macOS measurements.

## Verified preparation

The four budget tests pass, affected benchmark targets pass warnings-denied
Clippy, and formatting checks pass. An initial unused import in the test module
under Criterion's harness=false targets was corrected before measurement.
Both Linux trees pass the four budget tests and three existing perf_gate tests,
and all 135 original cases execute successfully in Criterion test mode.

The actual repaired Type3 probe uses ten samples, 100 ms warmup and an estimated
250 ms measurement, matching its CLI request. Both captured stages inventories
contain exactly 135 unique cases. Fourteen workload inputs (five shared source
files and nine PDFs) have identical hashes across the two trees.

Current whole-source fingerprint after the benchmark-only repair:
`cd4c6f1aa4ce74ef65b2564edc1c215a91689d185664385e0a581731ebbcc912`.
The control fingerprint with the same helper is
`000722df3bbb63d698a47d3963f601a7f31f3c1bcda93822898c493d75219692`.
The [binary reuse manifest](evidence/benchmark-budget-bindings.json) checks
nine Mac font executables and four Linux font executables: all are byte-identical
to their archived captures. Prior passes and failures remain unchanged. Source
verification confirms no production library or existing workload input changed.

## Completed first comparison

The fixed Type3 calibration passes at +0.69% (95% +0.05..+1.87%). All 21
full-inventory pairs completed without retries: **131 pass, four inconclusive,
zero classified regressions**. The complete matrix is not accepted yet.

| Stage | Pass | Inconclusive | Regression |
| --- | ---: | ---: | ---: |
| Parse | 27 | 0 | 0 |
| Interpret | 27 | 0 | 0 |
| CPU render | 23 | 4 | 0 |
| PNG encode | 27 | 0 | 0 |
| Total | 27 | 0 | 0 |

Every end-to-end case passes; the largest upper bound is +2.67%. The four
uncertain CPU-render results remain explicit:

| Case | Change | 95% interval |
| --- | ---: | ---: |
| curves @300 | -0.24% | -15.48..+19.97% |
| rect_fills @300 | -16.79% | -23.48..+11.26% |
| strokes @300 | -2.92% | -10.60..+14.12% |
| text_type3 @150 | +1.24% | -3.18..+5.62% |

The [complete table](PIPELINE_MEASUREMENTS.md) and
[paired export](evidence/pipeline-pairs.json) retain all 135 cases and 2,835
comparison pairs. The 834,510-byte export also retains the 21 calibration
pairs. An independent local audit recomputes every raw median, ratio and
bootstrap verdict and verifies all 84 process markers, AB/BA order, actual
sample/warmup counts, input hashes and executable identities. It verifies
5,712 raw sample files containing 57,120 samples. See
[the audit](evidence/pipeline-audit.json).

The original remote checkout remains clean at
`77732617a558e6d85c375cc6624d4d1c49ebe648`. All source fingerprints and six
archived executable hashes match their captures. The complete archive, including
raw observations, both stages binaries, preparation logs and the original
configuration probe, is mirrored locally:
`font-linux-recheck-20261009/pipeline-evidence.tar.gz`.
It contains 29,258,850 bytes, SHA-256
`75ed6c620cde0ec578a5f976e8761ffe959b4035b2a273908a0cc5da810052a8`.
The original font archive and macOS evidence are unchanged.

## Sampling-variance inspection

All uncertain cases use Criterion's `iter_batched(..., SmallInput)` CPU-render
loop. Criterion 0.8.2 retains the returned RGBA images until the timed batch
ends, with `ceil(sample_iterations / 10)` outputs per batch. Thus sampling
plans also change the number of live images and memory reuse; this is a
measurement property, not evidence of a font-code regression or improvement.

For each of the three 300 DPI cases, all 42 observations use 2, 4, ..., 20
iterations per sample. Their first five samples retain one output and their
last five retain two. Median sample costs rise from roughly 1.3–1.9 ms in
most early positions to 3.3–3.6 ms in later positions. Type3 @150 has two
sampling plans: 27 observations use 7, 14, ..., 70 iterations, with a median
285.97 us; 15 use 8, 16, ..., 80, with a median 310.93 us. These associations
motivate checking batching and allocator effects before committing to a large
extension. They do not establish causality or clear any uncertain result.
Every source observation is retained in
[the descriptive export](evidence/pipeline-variance.json).

## Preregistered output-retention diagnostic

Compile a separate diagnostic crate outside both frozen source trees. Use the
current implementation and the four uncertain fixtures/DPIs only. Both modes
use one exact archived executable and the same renderer closure, with renderer
creation and output destruction excluded from timing. The sole mode change is
Criterion's `SmallInput` versus `PerIteration` batch policy. Require complete
RGBA equality on repeated setup renders. Pin third-party versions to the
captured current lockfile and verify the original source, input and executable
identities before and after the experiment.

Run 21 fixed blocks with four processes per block: two repetitions of each
mode. Each process measures the four cases using ten samples, 0.1 s warmup and
0.25 s measurement. Alternate mode order and repetition-slot order by block.
Recompute same-binary paired intervals independently for each mode and case;
calibration requires both bounds within -5..+5%. Retain all 84 processes,
336 raw sample files and 3,360 samples, including any unsuccessful calibration.
Do not stop early based on intermediate results.

This isolates batching in a new four-case context; it is a diagnostic, not
production before/after acceptance. Never pool it with the original whole-suite
data or use a passing subset to relabel the first comparison. The original
135-case result remains 131 pass and four inconclusive until separate justified
acceptance evidence is established. Scripts and diagnostic crate are retained
under `font-linux-recheck-20261009/`, outside the shipping workspace.

## Completed output-retention diagnostic

All 21 blocks completed, with repeated-render RGBA equality checks and matched
third-party versions. The two modes use exactly the same diagnostic executable.
Both frozen production trees, their workload inputs and all six original
archived executables are unchanged. The independent local audit recomputes all
eight calibrations from 336 raw sample files / 3,360 samples and verifies the
84 completion markers. Full evidence: [pipeline-batching.json](evidence/pipeline-batching.json).

| Case | SmallInput same-binary 95% interval | Result | PerIteration same-binary 95% interval | Result |
| --- | ---: | --- | ---: | --- |
| curves @300 | -5.03..+6.90% | inconclusive | -1.03..+0.74% | pass |
| rect_fills @300 | -1.76..+0.88% | pass | -1.87..+0.80% | pass |
| strokes @300 | -3.86..+1.47% | pass | -1.51..+1.34% | pass |
| text_type3 @150 | -2.30..+2.37% | pass | +0.03..+1.77% | pass |

The separate four-case context differs from the original whole-suite process.
It reproduces one failed SmallInput calibration and establishes passing
PerIteration repeatability for these four cases in this context. A descriptive
post-run mode comparison shows costs changing by -50.63%, -5.09%, -4.05% and
-18.48%, respectively. These are effects of the benchmark's output lifecycle,
not production font improvements; they must not be reported as such.

The diagnostic supports investigating a bounded, one-output-at-a-time rendering
benchmark. Any resulting benchmark repair requires fresh control/current
measurements and explicit workload/source identities. It does not clear the
original four uncertain cases or prove that all their variance has one cause.
The original 131-pass/four-inconclusive result and the macOS heap-copy regression
remain unchanged. No production source or original stages workload was modified
for this experiment, and no benchmark process is left running.

The diagnostic archive is mirrored locally and remotely as
`font-linux-recheck-20261009/pipeline-batching-evidence.tar.gz`:
4,645,333 bytes, SHA-256
`e77f2215a084bb628ced7482d353eeda6a7de6148edb4632329a630150ab8ba7`.

## Preregistered bounded-output pipeline comparison

Change only the original CPU-render benchmark's batch policy to PerIteration,
identically in control and current trees. Its renderer setup, timed rendering
closure, output destruction boundaries, fixture bytes and 135 identities stay
the same. Each iteration now releases its output before the next begins.
Document this as a changed measurement protocol, preserving the SmallInput
archive, its four uncertain results and the independent diagnostic.

Archive fresh stages binaries and source fingerprints; verify exact unchanged
font binary identities before reusing prior font outcomes. Run the four budget
tests and three perf-gate tests on both sides, all 135 cases in test mode, and
warnings-denied Clippy for the changed benchmark. Check all 14 workload inputs
match between sides and the CLI still produces ten samples/100 ms warmup.

First run 21 fixed same-current-binary process pairs selecting the four
previously uncertain cases in one process by an anchored filter. Both slots
use the new stages binary and the same original target's untimed setup.
Require each case's paired 95% interval to lie within -5..+5%. This subset
calibrates those render cases; it does not establish complete acceptance.

If all four calibrations pass, run 21 fixed full-inventory control/current
pairs using the new stages binaries. Alternate AB/BA process order, retain
ten samples, 0.1 s warmup and 0.25 s measurement, and require all 135 identities
per process. Use the unchanged upper-95% +5% gate for all cases; uncertainty
is not a pass. No result-dependent stopping or extension. Archive all 5,838
raw sample files / 58,380 samples (168 calibration plus 5,670 comparison files)
when the planned run completes. Record partial data if calibration fails.

All new outcomes are reported separately. Never combine PerIteration timings
with SmallInput data or count the output-retention effect as a production
speedup. This is a repaired measurement of the same single-page rendering
operation; the original protocol's outcomes remain visible.

### Bounded-output preparation evidence

Both trees pass warnings-denied release Clippy, four budget tests, three
perf-gate tests and all 135 cases in Criterion test mode. The live configuration
probe verifies ten samples and the requested windows. The first preparation
attempt stopped before timing because Rust 1.96 lacked Clippy; its log is
preserved in `pipeline-bounded-preparation-attempt1`. Installing that component
did not change the default toolchain.

The current source fingerprint is
`b078a4d72637d8707f4ed0ab466be5d307a07f89b6e13892e63b2a2a581821d9`;
control is `f3d104568aefacbef59b225f7cc59196dec7fd9ee621039c4a531a5cd8a632a8`.
All 14 workload inputs match across sides. Fresh compilations prove all nine
Mac and four Linux font binaries remain byte-identical, retaining unsuccessful
outcomes too. See [the exact bindings](evidence/benchmark-bounded-bindings.json).
The new stages hashes are:

- Control: `3c635f4124178e007852686e6509756a4543ca85316f86b9b42e88d07bb900a8`.
- Current: `074fccd8100227ed7fb8aab56fefe121fdb39822fdf9c266b0902138121990e6`.

The fixed calibration has completed all 21 pairs and passes all four cases:
curves -0.67% (95% -1.09..+0.83%), rect_fills +0.13%
(-1.11..+1.10%), strokes +0.75% (-0.14..+1.51%), and Type3 +0.50%
(-1.25..+1.12%). The subsequent 21-pair full-inventory comparison is complete
under the separate `pipeline-bounded/` directory.

## Completed bounded-output comparison

**All 135 cases pass**, with no inconclusive cases or classified regressions.
Every stage passes all 27 cases. The largest upper 95% bound is +4.11%
(Type3 CPU rendering @96); the original +5% gate is unchanged.

The four previously uncertain workloads pass under the repaired protocol:

| CPU render case | Change | 95% interval |
| --- | ---: | ---: |
| curves @300 | +0.36% | -1.13..+1.33% |
| rect_fills @300 | +0.13% | -0.74..+1.36% |
| strokes @300 | -0.03% | -1.01..+0.97% |
| text_type3 @150 | +0.33% | -0.41..+1.06% |

See [all 135 rows](PIPELINE_BOUNDED_MEASUREMENTS.md),
[all paired observations](evidence/pipeline-bounded-pairs.json), and
[the independent audit](evidence/pipeline-bounded-audit.json). The local audit
recomputes all raw medians, ratios and bootstrap decisions, verifies all 84
completed processes and AB/BA ordering, and checks actual sampling budgets,
source/input/executable identities and all 5,838 raw sample files / 58,380
samples. The retained inventory includes 2,835 comparison case-pairs and
84 calibration case-pairs. No retry or observation is removed.

The old remote checkout remains clean at 77732617a558e6d85c375cc6624d4d1c49ebe648.
The original SmallInput archive and separate output-retention diagnostic stay
intact, with their original verdicts. Their timings are never pooled with this
new protocol. The passing result is a comparison of control/current under the
same corrected output lifecycle, not a speedup from changing the harness.
The historical macOS copy_heap regression remains a separate unresolved item.

Both hosts retain `font-linux-recheck-20261009/pipeline-bounded-evidence.tar.gz`:
25,185,198 bytes, SHA-256
`bf6e1b97aa7f3db0bedd37993aae439567c35db82d84d5b68968645e65bc6391`.
It preserves raw observations, both stages binaries, exact source changes and
backups, logs, preparation captures and the failed pre-timing Clippy attempt.
The fixed run is complete; no pipeline measurement process remains active.
