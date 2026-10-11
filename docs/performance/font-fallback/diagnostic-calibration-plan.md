# Fixed precision calibration

The remaining diagnostic gate is unresolved. Original data, thresholds and
workloads remain unchanged. This plan is recorded before the new measurements.

## Why more independent pairs

The old short-window default-scheduler same-binary calibration has a log-ratio
standard deviation of 0.3540; the current long-window calibration has 0.3543.
Both used only 21 independent process pairs. A rough normal-model precision
estimate for a sample median is

`n = ceil((1.96 * 1.253 * sd(log ratios) / log(1.05))^2)`.

These inputs suggest approximately 318–319 pairs for a 5% half-width. This is
planning evidence, not a claim of normality, independence or guaranteed power.
The actual decision still uses the existing paired bootstrap median interval.
Earlier different protocols/binaries are not pooled into the new result.

## Fixed experiment

- The exact current diagnostic executable is used on both sides:
  SHA-256 `9811213d8b861a759fe75dcc886c2106dda6e5791589973ef02071fbe5f37450`.
- Case: `font_diagnostics/copy_heap`; the 21,088-byte fixture is unchanged.
- Exactly 512 alternating AB/BA pairs, ten samples, 0.1 s warmup and 0.25 s
  measurement under the default scheduler on both sides.
- No early stopping or extension based on intermediate results.
- Same-binary calibration requires the complete 95% interval inside
  0.95–1.05. A directional speedup does not pass calibration.
- Output: `font-copy-precision-calibration.json`, with every raw pair
  retained and atomic checkpoints available if interrupted.

This is a calibration subset, not full-suite acceptance. If it remains
inconclusive, no new pass is inferred. If it calibrates successfully, plan a
fixed, sufficiently powered before/after comparison for the unresolved cases,
retaining all same-protocol observations. Any classified regression still
requires investigation. The 125 passing functional controls are unchanged.

## Completed calibration and fixed comparison plan

The complete 512-pair calibration passes: +1.04%, 95% -0.49..+3.17%, inside
the required two-sided 5% band. The report remains marked incomplete as a
suite because it selects one calibration case; this is not campaign acceptance.

Before starting the next comparisons, the same planning estimate was applied
to all 63 retained observations for each unresolved case:

| Case | Log-ratio standard deviation | Approximate pairs | Fixed additional pairs |
| --- | ---: | ---: | ---: |
| construct_owned | 0.5570 | 787 | 1,024 |
| copy_aligned | 0.3752 | 357 | 512 |
| copy_heap | 0.4026 | 411 | 512 |
| copy_unaligned | 0.3522 | 315 | 512 |

The additional measurements keep ten samples, 0.1 s warmup, 0.25 s measurement,
default scheduling, alternating AB/BA order and the unchanged 5% gate. Order:
copy_heap, construct_owned, copy_aligned, copy_unaligned. Every planned case
completes its fixed count; no early stopping or optional extension is used.

The original before and after executable captures remain the measurement
identities so all earlier same-protocol observations can be pooled unchanged.
The current source binding proves the after executable is byte-identical to
the writer-preflight candidate. Pool the original complete 21-pair report,
the retained 42-pair confirmation, and every new completed case report; never
pool calibration data into the before/after comparison. Thus each unresolved
case retains 63 earlier pairs in addition to its fixed new count. Seven passing
diagnostic cases retain their complete first-run observations.

Outputs use `font-diagnostics-precision-<case>.json`; the final complete
pooled report is `font-diagnostics-precision-pooled.json`. All earlier
regression and uncertainty evidence remains archived regardless of outcome.

## File-identity control

The fixed before/after run still classifies copy_heap as a regression, while
the same-path, identical-binary calibration passes. A separate control now
tests whether using distinct executable archives contributes to that difference.

Create two copies of the exact current diagnostic executable under fresh
`run-xxxxxxxx/executable/font_diagnostics` paths of equal length. Preserve
all executable bytes, permissions and capture input/environment metadata.
Derived captures explicitly record archive-copy provenance and do not claim
new compilation or measurements. Hash equality activates the existing
two-sided calibration rule.

Before measuring, fix exactly 512 AB/BA pairs for copy_heap, ten samples,
0.1 s warmup, 0.25 s measurement and default scheduling. Retain all results,
with no early stopping. Output: `font-copy-path-calibration.json`.
Neither this control nor its samples are pooled with actual before/after
comparisons. It does not modify either original archived executable.

The complete file-identity control passes: -0.58%, 95% -3.07..+0.13%, with
all 512 pairs retained. Distinct file paths alone do not explain the observed
before/after regression. This does not clear that regression or establish
full-suite acceptance.

## Process-local allocator intervention (diagnostic only)

Inspection of the installed libsystem_malloc (UUID
`971CDCFE-4D20-300E-A1D6-EC8672FDD258`) corroborates the type-hash and bucket
selection operations in the pinned Apple source. For these two allocation
sites, the type hashes differ by 1,498. The before binary's fixed bucketing
keys allow bucket steps of 3 or 0 modulo four; the after binary's keys allow
only 2 or 3. This explains why only the before binary can place these two
allocations in the same xzone. It does not yet establish a timing cause.

Before collecting new timings, fix 16 alternating AB/BA pairs for each of
three conditions: untouched keys, same bucket, and separate buckets. Rotate
condition order across pairs. Every process runs the original archived
executable and case, with ten samples, 0.1 s warmup and 0.25 s measurement.
ASLR and scheduling remain at their defaults. No source, executable bytes,
fixtures, environment allocator options, or system settings change.

LLDB stops immediately before the source allocation's bucket lookup. The
untouched condition reads metadata only. The other two conditions replace
only this disposable process's 16-byte xzone bucketing keys: `(0, 0)` forces
untyped allocations into the same pointer bucket, while `(2^32, 0)` selects
`type_hash mod 4`, placing these callsites two buckets apart. Both binaries
receive the same intervention. All breakpoints are removed before warmup;
the debugger remains attached without timed-region stops. A failure to write
or validate process memory aborts that attempt and is recorded.

Keep all 96 launches and all ten samples per launch. These are controlled
diagnostic timings, not acceptance measurements, and are never pooled with
the original 575 pairs. Sixteen pairs may leave wide intervals; no extensions
or early stopping are selected from intermediate outcomes. The aim is to test
whether bucket placement changes copy cost, not to make the strict gate pass.
Output: `font-copy-allocator-intervention/summary.json`.

The 96-launch experiment completed. Separate-versus-same bucket rules increase
median copy cost by 95.02% in the before binary (95% +62.98..+115.83%) and
101.51% in the after binary (+77.87..+127.34%). Before/after comparisons under
identical rules have wide intervals and do not establish strict equivalence.
Because this intervention changes later allocations too, it does not isolate
the source allocation from other allocation-context effects.

## Single-allocation type intervention (diagnostic only)

To isolate that ambiguity, a separate experiment changes only the source
allocation's type descriptor at the entry to `xzm_malloc`. Original bucketing
keys, subsequent type descriptors and both archived executables stay unchanged.
For each process, predict the destination's descriptor as the observed source
callsite descriptor plus 1,498, and apply the already corroborated bucket
formula. The same-bucket condition substitutes that destination descriptor for
the source; the separate-bucket condition chooses a descriptor mapped two
pointer buckets away. Untouched leaves the original source descriptor intact.
Verify the source's actual xzone at allocator entry and its returned pointer,
then remove all breakpoints before Criterion warmup. Verify keys are unchanged.

Fix 16 AB/BA pairs per condition, rotating conditions as above: 96 new launches,
with all samples retained separately from every earlier experiment. Keep the
same original benchmark arguments and LLDB timing restrictions. This is a
narrower causal diagnostic, not an extension selected to force a passing
before/after interval, and no timing verdict replaces the original gate.
Output: `font-copy-source-type-intervention/summary.json`.

The single-allocation experiment completed all 96 successful launches. Separate
versus same source buckets increases copy time by 93.71% before (95%
+66.17..+115.05%) and 71.10% after (+57.35..+87.06%). One additional attempt
failed before warmup because descriptor zero uses fallback entropy. After
excluding zero from candidate search, only unmeasured launches resumed;
all 59 completed launches were retained. One earlier zero-descriptor launch
had verified the desired actual xzone and remains explicitly identified.
All observations, both source versions and the failed log are archived. No
before/after comparison establishes strict equivalence, and no acceptance
result changes.
