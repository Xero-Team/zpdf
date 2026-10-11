# Copy diagnostic investigation

The four copy controls are retained. Their timings are not automatically
classified as production font regressions, and uncertain timing results do
not become passes because their source is unchanged.

## Observations

The earlier v4 pooled copy_heap result was +16.33% with a paired 95% interval
of +6.58..+26.95%, which is a classified regression under the strict gate.
An identical-binary calibration was itself inconclusive: +3.23%, with a
-12.25..+27.40% interval. The preceding shared-registry candidate measured
+37.19%, with a +1.34..+50.48% interval, also inconclusive. Individual process
medians often ranged from roughly 300 ns to 670 ns for the same 21,088-byte
allocation, copy and destruction operation.

These observations are preserved in:

- `font-final-v4-diagnostics-pooled.json`
- `font-final-v4-copy-calibration.json`
- `font-shared-lock-diagnostics-paired.json`

They demonstrate that process-to-process variability requires investigation.
They do not identify its cause. Allocation placement, process layout and host
scheduling remain hypotheses, not established explanations.

The raw Criterion samples also vary within a process. In the identical-binary
calibration, the median within-process sample coefficient of variation is
29.23% on side A and 30.04% on side B; between-process coefficients are 23.67%
and 25.83%. Some ten-sample sequences move from about 295 ns to 500–680 ns
within one measurement window. A fixed address/layout difference alone cannot
explain that temporal variation. If the current short-window run remains
uncertain, first calibrate a longer window using identical binaries. Any changed
window is a separate protocol and must not be pooled with the existing data.

## Exact current-binary inspection

The original diagnostic executable and the current adjacent-repeat executable
have the same shared Criterion iteration function for copy_static, copy_heap,
copy_aligned and copy_unaligned. There are 93 AArch64 instructions in each.
After resolving relocated external call targets to their symbols, every
instruction matches. Loop position and internal branch targets also match.

The timed operation calls Rust allocation/deallocation wrappers and system
memcpy. It does not call zpdf font parsing, mapping or fallback. Criterion's
timer calls and allocation error paths also match. This rules out added font
work in the copy loop, but does not rule out a changed process layout affecting
allocation or copying.

Evidence:

- `font-repeat-copy-assembly.json`: executable hashes, function bounds,
  normalized instructions, resolved calls and equality result.
- `font-repeat-copy-before.asm`
- `font-repeat-copy-after.asm`

The inspection is read-only and did not change source, fixtures, allocators,
environment variables or benchmark thresholds. It is diagnostic evidence,
not performance acceptance. The full current diagnostic suite and any fixed
confirmation must retain all observations before the final decision.

## Current confirmation and longer-window calibration

The current 63-pair pooled copy_heap result is +28.64%, 95% +7.38..+45.78%,
classified as a regression. It remains visible. Three other current diagnostic
cases remain inconclusive; seven pass. Report:
`font-repeat-final-diagnostics-pooled.json`.

A fixed identical-current-binary calibration increased warmup from 0.1 s to
1 s, measurement from 0.25 s to 2 s, and samples from ten to 50, with 21 pairs
and default scheduling. It remained inconclusive: +1.37%, 95% -7.40..+16.96%.
Report: `font-repeat-copy-long-calibration.json`. The changed protocol
is separate and is never pooled with the original observations. Longer windows
alone did not establish a stable calibration, so a full long-window acceptance
campaign is not justified by this result.

The next fixed experiment uses the same current executable on both sides,
21 pairs, ten samples, 0.1 s warmup, 0.25 s measurement and background scheduling.
It tests whether the scheduling policy affects copy stability on this
heterogeneous-core host. It does not assume that the policy is the cause, and
background results cannot be presented as default-scheduler acceptance.

The fixed background calibration also remained inconclusive: -3.42%, with
a -12.43..+3.46% interval. The identical-binary calibration requires both
directions to stay within 5%, so its lower bound prevents a pass. Report:
`font-repeat-copy-background-calibration.json`. Neither changing the
window nor the scheduler established a stable calibration.

An aggregate host observation during the later writer-preflight controls showed
72.0–76.59% CPU idle, a load average near 2.6, 15 GB used memory and about 6.2 GB
in the compressor. No additional swap-ins or swap-outs occurred across the two
snapshots. pmset reported no recorded thermal/performance warning and no CPU
power status. This is environmental context, not a causal explanation or proof
that the host was fully idle during the earlier copy measurements.

## Standalone allocation/copy probe

A standalone Rust program uses the same 21,088-byte fixture, without any zpdf
code. It separates allocation/deallocation, copying into an already allocated
buffer, and heap cloning. Four fixed pairs alternate inherited QoS and explicitly
requested user-initiated QoS. Each process warms each stage for 200 ms and records
32 batches of 50,000 operations, sampling destination addresses outside the
steady part of each batch. It is diagnostic only, not an acceptance benchmark.

The inherited requested QoS is 33 (user-interactive), not an unspecified low
priority. The alternate requested QoS is 25 (user-initiated). Neither setting
removes the copy variation:

| Mode/stage | Median ns | Range ns | Batch coefficient of variation |
| --- | ---: | ---: | ---: |
| Inherited / allocate+free | 18.35 | 17.11–20.30 | 5.05% |
| Inherited / preallocated copy | 316.32 | 248.12–620.17 | 37.20% |
| Inherited / clone heap | 458.80 | 292.43–686.94 | 29.12% |
| User-initiated / allocate+free | 19.00 | 17.33–23.05 | 5.14% |
| User-initiated / preallocated copy | 367.53 | 231.24–621.72 | 35.15% |
| User-initiated / clone heap | 601.85 | 258.25–682.88 | 24.51% |

Each batch's sampled destinations reuse one address. Source addresses are
16-KiB aligned; clone destinations have offsets zero or 8,192. Preallocated
source/destination offsets are both zero. Copy variation therefore remains
when allocation is removed from the timed loop and buffers are already resident.
This narrows the investigation beyond allocator cost, but does not identify
hardware scheduling, cache, bandwidth or another system mechanism as the cause.

Evidence: `font-copy-probe.rs`, `font-copy-probe-v2-summary.json`
and `font-copy-probe-v2-results/`. Source/executable hashes are retained.
The [source snapshot](evidence/copy-probe-source.rs.txt) and
[all probe observations](evidence/copy-probe.json) are also retained with this
report. To reproduce, copy the source snapshot to `font-copy-probe.rs`
and compile with `rustc --edition 2024 -C opt-level=3`; run `normal` and
`user_initiated` in alternating order. Source paths in include_bytes are
relative to that target-directory location.
The initial prototype had an incorrect QoS getter signature and exited before
emitting data; its empty attempt is explicitly excluded. The corrected getter
matches the local SDK's pthread/qos.h. Only the standalone probe thread's QoS
was changed. Library source and original benchmark inputs were untouched.

All 125 current functional font performance controls pass, including complete
font construction and rendering. The four unresolved diagnostics remain visible
and keep full campaign acceptance open. These findings do not relabel any
inconclusive result as a pass or erase the classified copy regression.

## Buffer-layout matrix and precision plan

The next standalone experiment copies the same 21,088 bytes between preallocated
buffers. It tests destination offsets 0, 16, 64, 128, 256, 512, 4,096, 8,192 and
16,384 with 24 rotated/reversed rounds in each of four processes. Every copy is
checked against the source. All 864 samples are retained in
[copy-layout.json](evidence/copy-layout.json); the exact
[source](evidence/copy-layout-source.rs.txt) is also retained.

Within this matrix, offset medians range from 169.22 to 185.60 ns. Every offset
still has substantial variation; batch coefficients of variation range from
16.27% to 67.07%. No tested offset establishes a stable replacement measurement
condition. This probe has different buffer capacities and loop structure from
the earlier probe, so their absolute timings are not a before/after comparison.

The old short-window same-binary calibration also shows an order association:
AB pairs have a median ratio of 1.412, versus 0.829 for BA pairs. The longer-window
and background calibrations do not show the same clear pattern. This is a
descriptive observation, not grounds to change the estimator or discard samples.

Observed default-scheduler log-ratio standard deviations of about 0.354 suggest
roughly 318–319 pairs for an approximate 5% median-interval half-width under a
simple planning model. A fixed 512-pair current-binary calibration completed
according to the preregistered
[precision plan](diagnostic-calibration-plan.md). It retains the original
workload, default scheduler and 5% threshold. There is no early stopping or
extension based on intermediate results. The actual bootstrap interval, not
the planning approximation, determines the result. It passes at +1.04%, with
a -0.49..+3.17% interval. This establishes calibration for the copy case under
the default protocol, not a full-suite pass.

The next preregistered before/after comparisons add 1,024 pairs for
construct_owned and 512 each for copy_heap, copy_aligned and copy_unaligned,
retaining all 63 earlier pairs for each. Their counts follow the same variance
planning rule. Source, fixture, measurement windows, scheduling and thresholds
remain unchanged. Calibration samples are never mixed into those comparisons.

## Completed precision comparisons

All preregistered comparisons completed. Pooling the original 21 pairs, the
42-pair confirmation and the fixed additional measurements retains every
observation and yields ten diagnostic passes and one regression:

| Case | Total pairs | Change | 95% interval | Verdict |
| --- | ---: | ---: | --- | --- |
| construct_owned | 1,087 | -0.78% | -1.44..-0.10% | pass |
| copy_aligned | 575 | +0.15% | -0.80..+1.41% | pass |
| copy_heap | 575 | +16.86% | +10.40..+26.20% | regression |
| copy_unaligned | 575 | -0.11% | -2.41..+1.10% | pass |

The seven other diagnostic cases retain their original passing observations.
Pooled report: `font-diagnostics-precision-pooled.json`. Across the
entire current inventory, 135 pass and one remains a regression; no case is
inconclusive. Sample count alone does not clear the heap-copy difference.

## Initialization and system-copy inspection

Read-only inspection shows the same main-entry address, 8,496-byte save/frame
allocation, 21,088-byte source allocation and requested alignment one before
and after. There are no direct font calls in the initialization prefix through
copy_heap registration. Both static source addresses have offset 36 modulo 64,
although their offsets modulo 128 differ. Evidence:
`font-copy-initialization.json` and its before/after assembly files.

The public memcpy and memmove symbols in a separate diagnostic process resolve
to the same system implementation. For nonoverlapping buffers, its inspected
AArch64 code selects STNP non-temporal store hints at lengths of at least
16 KiB; shorter copies use STP. Store hints alone do not prove actual cache
behavior or explain a before/after difference.

A fixed independent probe tests nine sizes and both direct copying and 8-KiB
chunked copying, with 24 rotated/reversed rounds in four processes. All 1,728
samples and the exact [source](evidence/copy-threshold-source.rs.txt) are retained
in [copy-threshold.json](evidence/copy-threshold.json). Direct-copy medians at
16,383 and 16,384 bytes are essentially equal (165.75 ns each), so this probe
does not show a sharp threshold discontinuity. At 21,088 bytes, direct copying
is 208.72 ns versus 222.36 ns for chunked copying. The latter has lower variation
in this probe but is slower; no production or benchmark implementation was
changed on that basis.

The remaining fixed control compares two separate, equal-path-length archives
of identical executable bytes. It tests executable file identity independently
from code changes. Its 512 pairs, original protocol and two-sided 5% calibration
gate were preregistered in [the plan](diagnostic-calibration-plan.md).
`font-copy-path-calibration.json` remains separate from actual
before/after evidence. Full campaign acceptance remains open.

The complete file-identity control passes: -0.58%, 95% -3.07..+0.13%, over
512 pairs. Its [retained observations](evidence/copy-path-calibration.json)
do not reproduce the before/after gap merely by placing identical bytes in
different equal-length paths. This does not rule out every possible path or
layout interaction, and does not clear the actual regression.

The latest [complete table](PRECISION_MEASUREMENTS.md) and
[evidence export](evidence/writer-preflight-precision-pairs.json) contain all
135 passes and the one remaining regression, including every earlier pair.
No experiments remain running. The source, gate and original workloads are
unchanged; further work must explain or remove the specific before/after gap
without treating a calibration pass as proof of implementation equivalence.

## Observed allocation placement

LLDB inspections use the original archived binaries with ASLR enabled. The
copy calls request 21,088 bytes; both allocations have a 24,576-byte usable
size and report DefaultMallocZone. Both binaries use SDK 26.5, the same
minimum OS version and linked libraries. Their immediate allocation caller
addresses match relative to the executable; the Rust allocation shims tail
call the system allocator. Their code-signature CDHashes differ.

In six alternating launches per binary, the before binary places destination
24 KiB after source in four launches; the after binary does so in none. A
separate debugger association run retains all ten samples and explicitly flags
the interrupted first sample. Two close-layout before launches have descriptive
medians near 251 ns; other before and after layouts overlap around 293–342 ns.
These debugger timings are excluded from acceptance.

- [Address observations](evidence/copy-lldb-layout.json).
- [All association samples](evidence/copy-lldb-association.json).
- Original usable-size inspection: `font-copy-lldb-size/{0,1}.log`.

A separate preallocated-arena probe varies eight distances and both directions
over 24 rotated/reversed rounds in four processes. All 1,536 samples are retained
in [copy-gap.json](evidence/copy-gap.json), with its
[source](evidence/copy-gap-source.rs.txt). Group medians span 167–176 ns. Merely
changing virtual distance inside one already allocated arena does not reproduce
the archived-program difference; allocation context still matters.

## Corroborated allocator bucket mechanism

The [pinned Apple source manifest](evidence/apple-allocator-source-manifest.json)
records libmalloc revision `c49dafa25f1efe8607701ae6014a663ad2ee437f`. Its
[bucket selection](https://github.com/apple-oss-distributions/libmalloc/blob/c49dafa25f1efe8607701ae6014a663ad2ee437f/src/xzone_malloc/xzone_malloc.c#L89)
mixes a type descriptor with executable-specific keys. Its
[initialization](https://github.com/apple-oss-distributions/libmalloc/blob/c49dafa25f1efe8607701ae6014a663ad2ee437f/src/xzone_malloc/xzone_malloc.c#L7723)
derives those keys from a kernel-provided hash of the boot session and main
executable CDHash. This does not assert that the entire public source revision
matches the installed operating system.

The installed libsystem_malloc UUID is
`971CDCFE-4D20-300E-A1D6-EC8672FDD258`. Its actual instructions corroborate the
relevant operations: ordinary malloc uses the caller PC shifted right by two
as a 32-bit fallback type hash; xzm selects this size class using four pointer
buckets and the high 32 bits of a keyed multiply-add. Installed OS disassembly
remains in ignored `font-copy-allocator-inspection/lookup-assembly.log`.

Two sets of six alternating launches per binary record the actual allocation
descriptors and xzone indices. The second set reads the bucket metadata and
independently computes each expected xzone: all 24 predictions match. Key
fingerprints are stable across six launches of each executable and differ
between executables. Raw key material is not exported. Evidence:
[initial inspections](evidence/copy-allocator-inspection.json),
[model checks](evidence/copy-allocator-model.json), and
[LLDB reproducer](evidence/copy-allocator-lldb.py.txt). The first smoke attempt
stopped at an unrelated Criterion allocation (80 bytes) before the timed loop;
it failed its size assertion and produced no measurement. The corrected
reproducer first stops at the exact 21,088-byte destination callsite.

The source and destination caller PCs are 5,992 bytes apart, so their type
hashes differ by 1,498 throughout the inspected ASLR range, without a 32-bit
hash wrap. With the actual keys, their bucket
step modulo four can be:

| Executable | Possible destination-minus-source bucket step | Same bucket possible? |
| --- | --- | --- |
| Before | 3 or 0 | Yes |
| Current | 2 or 3 | No |

These possibilities follow from 64-bit multiply-add arithmetic and its low-half
carry, not from an assumed ASLR distribution. If that low half were uniform,
the before same-bucket fraction would be about 36.57%; uniformity has not been
established, so this is not a measured frequency estimate. The exact current
keys forbid a same-bucket outcome for these callsites. This establishes a
mechanism for the allocation-layout difference, independently of font calls.

## Process-local bucket intervention

The [preregistered experiment](diagnostic-calibration-plan.md) runs 16 AB/BA
pairs in each of three conditions, rotating their order: untouched keys,
constant same-bucket keys, and deterministic separate-bucket keys. Changes
apply only to disposable test processes. Original executable bytes, source,
fixtures, ASLR, scheduler and the acceptance dataset remain unchanged. LLDB
remains attached, but all breakpoints are removed before warmup and timing.

All 96 launches completed, retaining all 960 samples. The within-binary
separate-versus-same rule contrast is large in both binaries:

| Diagnostic contrast | Change | Paired 95% interval |
| --- | ---: | --- |
| Before: separate vs same rule | +95.02% | +62.98..+115.83% |
| Current: separate vs same rule | +101.51% | +77.87..+127.34% |
| Current vs before: untouched | +22.55% | -4.25..+86.38% |
| Current vs before: same rule | -7.19% | -11.48..+7.69% |
| Current vs before: separate rule | +0.68% | -1.92..+5.22% |

The controlled rule affects copy-loop cost in both binaries. However, it also
changes later allocations, so it does not isolate one buffer from the rest of
the allocation context. The before/after intervals do not establish equivalence
within the original gate, and none of these timings is acceptance evidence.
All data and source are retained in
[the intervention export](evidence/copy-allocator-intervention.json),
[LLDB module](evidence/copy-allocator-intervention.py.txt), and
[runner](evidence/copy-allocator-run.py.txt). Restore the source snapshots to
their original build-output filenames before running the Python driver from the
repository root. Archived binary paths and the exact allocator UUID are checked;
the private offsets require a fresh review on another OS build.

## Single source-allocation intervention

A second fixed experiment changes only the source allocation's type descriptor
at xzm entry. It leaves the process's original bucketing keys and all later
allocation descriptors intact. The same-bucket condition uses the predicted
destination descriptor; the separate-bucket condition chooses a source
descriptor two buckets away. The actual source xzone is checked before
allocation returns, and every breakpoint is removed before warmup. As in the
earlier inspections, the destination descriptor follows from the unchanged
caller PC; no timed-loop instrumentation is added.

All 96 planned successful launches and their 960 samples are retained. One
additional attempt failed a source-xzone assertion before warmup: the initial
candidate search included descriptor zero, which means fallback entropy to
the allocator. It produced no timed samples. The search now starts at one;
the run resumed with all 59 previously completed launches intact. One earlier
successful zero-descriptor launch remains included because its actual checked
xzone matched the requested separate-bucket condition. Its special descriptor
is explicitly recorded; it is not silently discarded or treated as an ordinary
hash prediction. The failed log and both module versions are archived.

| Diagnostic contrast | Change | Paired 95% interval |
| --- | ---: | --- |
| Before: separate vs same source bucket | +93.71% | +66.17..+115.05% |
| Current: separate vs same source bucket | +71.10% | +57.35..+87.06% |
| Current vs before: untouched | +19.45% | -2.10..+65.10% |
| Current vs before: same source bucket | +7.14% | -3.71..+20.99% |
| Current vs before: separate source bucket | -0.91% | -13.39..+6.64% |

Changing the bucket of this single allocation is sufficient to change the
copy workload substantially in both executables. Together with the observed
keyed bucket differences, this identifies an allocator-mediated contributor
to the archived before/after difference. It does not prove that this mechanism
accounts for exactly all of the original +16.86%, nor do these 16-pair intervals
establish before/after equivalence within 5%. It does not identify the underlying
cache or memory-system mechanism. The same-arena distance probe did not
reproduce the effect by virtual distance alone.

Evidence: [all source-type observations](evidence/copy-source-type-intervention.json),
[current LLDB module](evidence/copy-source-type.py.txt),
[resumable runner](evidence/copy-source-type-run.py.txt), and
[attempt/source provenance](evidence/copy-source-type-provenance.json).

Production source remains
`11f20688d746c1f2ed3883588023be862d4821293372e8c3e02fc0ee5f84aa67`.
No allocator override is proposed for zpdf, and no original benchmark workload
was changed. All experiments have completed. The original 575-pair regression
and complete **135-pass / one-regression** gate remain unchanged; these new
diagnostics supply causal evidence, not a replacement acceptance protocol.

The [evidence audit](evidence/copy-allocator-evidence-audit.json) recomputes all
ten comparison summaries from the 1,920 retained intervention samples,
verifies the 24 allocator predictions, checks the gap-probe source hash and
failure provenance, and confirms unchanged production/executable fingerprints.
Python source snapshots parse and `git diff --check` passes. Production suites
were not repeated for these documentation/diagnostic-only changes; their
existing frozen-source validation remains applicable.

## Allocation-callsite follow-up

A separate fixed probe now tests whether using one allocation callsite for
both the source and destination is enough to obtain stable copy measurements.
The two optimized binaries have identical helper instructions but distinct
CodeDirectory hashes. All 168 processes and 1,680 raw samples are retained.
All four first-run same-binary calibrations were inconclusive, including both
shared-callsite conditions. The fixed precision extension retained all 21
blocks and added 491, then passed all four probe calibrations with 512 pairs
per condition. Both shared and distinct modes pass; the probe therefore does
not show an exclusive benefit from unifying callsites. This prospective
benchmark repair is not adopted. See [the first-run results](copy-callsite-results.md)
and [precision results](copy-callsite-precision-results.md). The original
575-pair regression and acceptance threshold remain unchanged.
