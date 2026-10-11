# Independent Linux recheck

**Completed: all 57 common Linux benchmark cases pass the unchanged 5% gate.**
Release native validation reports 788 passes across 42 suites. All fixed
measurement stages have ended; source and executable hashes remain unchanged.
This is independent Linux evidence. The historical macOS copy_heap regression
remains recorded under its original source/host/protocol identity.

The user authorized testing in a remote development container, using its
existing zpdf location. Access is through the container's `dev` SSH account.
The existing checkout is clean at `77732617a558e6d85c375cc6624d4d1c49ebe648`
in the container's project directory; preserve it and its caches.

## Scope and source identity

The frozen current implementation has source fingerprint
`11f20688d746c1f2ed3883588023be862d4821293372e8c3e02fc0ee5f84aa67`.
The independent control uses production commit
`137ab2b7ea6c0e1bd02624a7ec8a36d57f836943`, with benchmark-only additions and
identical fixtures/tooling. Both start from the current pinned Cargo.lock;
the control requires the workspace-only lock adjustment documented below.

This is a new baseline-to-current comparison. The existing reference worktree
does not reproduce the later intermediate source fingerprint of the archived
macOS diagnostic baseline. Do not claim that rebuilding it reproduces that
binary. Linux results remain separate from all macOS pairs and do not erase
the recorded macOS copy_heap regression.

Prepare separate `control` and `current` repositories below the old checkout's
`font-linux-recheck-20261009/`, with a task-owned shared Cargo build cache.
Transfer repository source and fixtures, not local macOS executables or ignored
private inputs. Record source/lock/workload/executable hashes for every capture.
Verify the control has no production changes relative to its named commit and
the current source fingerprint matches the frozen local implementation.

## Initial environment inspection

- Linux 6.1.0-37-amd64, x86_64, Intel Xeon Gold 6133 at 2.50 GHz.
- 40 visible logical CPUs; Microsoft hypervisor reported by lscpu.
- No cgroup CPU or memory maximum reported; effective CPU set 0-39.
- Approximately 32 GB RAM and 5.7 GB free overlay storage at inspection.
- Existing Rust 1.96.0 is available; the default 1.91.1 is older. Select one
  compatible toolchain explicitly for both builds and record its exact version.
- Use bounded build concurrency and the shared cache; do not remove the user's
  old target directory or change their default toolchain.

## Preregistered first measurements

First compile and validate the current and control source trees on Linux.
Run the common font workload correctness checks before capture. The matching
comparison inventory is `font_diagnostics` (11) plus `fonts` (46), including
all four copy controls and both Unicode variation queries. New feature suites
remain covered by their existing frozen-source evidence and receive applicable
Linux correctness checks; unsupported old APIs are not fabricated as baselines.

Run a fixed 21-pair identical-current-binary calibration for copy_heap first.
Use ten samples, 0.1 s warmup, 0.25 s measurement, alternating AB/BA order and
the default scheduler. Calibration requires the entire paired bootstrap 95%
interval within 0.95–1.05. If it passes, run each common comparison case for
21 fixed pairs under that same protocol. The original +5% upper-bound gate
applies; uncertainty is not a pass. Retain every observation and do not pool
different hosts or protocols. Any further confirmation is planned separately
after inspecting the completed first-run evidence.

Record container resource limits, libc, CPU identity, toolchain, available
storage and any GPU adapter before interpreting results. Software GPU timing
does not stand in for hardware GPU acceptance. No timing claims have been made
by this preparation document.

## Verified setup

The isolated checkouts are created. Remote current source exactly matches the
frozen fingerprint above; the independent control fingerprint is
`55958f86c1dcbb5f727135fb264578c947b25d28b05605afa03f116d138f8217`.
The control's only tracked changes are its benchmark Cargo.toml and benchmark
library module declaration; untracked additions are benchmark tooling and
fixtures. Its production source remains the named baseline commit.
Every transferred file and both initial lockfiles were checked against the
transfer manifest. The original checkout remains clean at its original commit.

Both builds initially select Rust 1.96.0, LLVM 22.1.2, with four compilation
jobs and the task-owned shared target directory. The runtime is glibc 2.35.
Current fixture regeneration checks pass. The initial offline capture stopped
before compilation because the remote crates.io cache lacked rsa; its log is
preserved separately and contains no measurements. Online locked acquisition
and the first release build subsequently succeeded without changing the lock.
All three shared font workload tests passed. Current diagnostic and original
font inventories have been captured (11 and 46 cases respectively); these are
executable identities, not timing results. The current Linux diagnostic binary
SHA-256 is
`8a33e0ed72b2da4e3ea0a73c99745b0b85c6964bc020927d793a5c4d50c39009`.

The broader release-mode native command passed, reporting **788 passes across
42 suites**, including zero-test binaries. Missing private-input early returns
are not corpus coverage. Each command, return code and log hash is recorded
in `campaign-progress.json`. No GPU device
was found at `/dev/dri` or `/dev/dxg`; this campaign makes no hardware GPU
performance claim.

## Completed calibration and first diagnostic comparison

The fixed 21-pair identical-current-binary copy_heap calibration passes at
-0.12%, with a paired 95% interval of -1.18..+1.34%. Its case verdict passes
the two-sided 5% band; the report's full-inventory status remains not_accepted
because calibration selects just one of eleven cases. The driver checks the
case verdict explicitly and does not interpret that partial report as a full
suite pass.

The 11-case baseline-to-current diagnostic comparison has completed its fixed
21 pairs per case: **seven pass, four inconclusive, no classified regressions**.
In particular, copy_heap passes at **-0.13%, 95% -1.31..+1.01%**. This provides
an independent Linux result; it does not rewrite the recorded macOS result.

| Inconclusive diagnostic | Change | Paired 95% interval |
| --- | ---: | --- |
| construct_owned | +2.54% | -3.83..+10.89% |
| copy_aligned | +2.45% | +0.33..+6.42% |
| copy_static | +3.60% | +1.52..+6.04% |
| drop_loaded | +2.44% | +0.25..+6.75% |

The separate 46-case original-control comparison also completed: 45 pass and
one is inconclusive. Mixed-supplementary extraction is +4.90%, with a paired
95% interval of +3.83..+6.23%. Thus all **57 first-run cases are complete: 52
pass, five inconclusive, no classified regressions**. The full Linux recheck
is not yet accepted. All rendering, end-to-end and writer controls pass.

## Fixed precision extension

This extension is recorded before new timing starts. It retains all 21 earlier
pairs for each selected case and uses the same captured binaries, inputs,
toolchain, scheduler, windows and 5% gate. No case is removed or relabeled.

For planning, use `ceil((1.96 * 1.253 * sd(log ratios) / log(1 + h))^2)` as a
rough total-pair estimate for a median interval half-width. Target h=0.02 for
the four diagnostics and h=0.003 for extraction, whose point estimate is close
to the gate. This approximation is not an independence/normality guarantee;
actual decisions still use the existing paired bootstrap. Subtract the 21
retained pairs, round the additional count up to a power of two, and use at
least 64 additional pairs.

| Case | Observed log-ratio SD | Target half-width | Fixed additional pairs |
| --- | ---: | ---: | ---: |
| construct_owned | 0.130713 | 2% | 256 |
| copy_aligned | 0.051537 | 2% | 64 |
| copy_static | 0.045246 | 2% | 64 |
| drop_loaded | 0.085586 | 2% | 128 |
| extract/mixed_supplementary | 0.020021 | 0.3% | 256 |

Run in table order, ten samples, 0.1 s warmup, 0.25 s measurement and alternating
AB/BA order. Complete every fixed count; no early stopping or extension based
on intermediate results. Pool each suite's complete first report with its
completed extension reports using the existing identity/protocol checks. Keep
calibration and all macOS data separate. A remaining inconclusive result is
still not a pass; this experiment is intended to improve precision, not to
guarantee a favorable decision.

The [first-run evidence export](evidence/linux-recheck-first-pairs.json)
retains all 57 case outcomes, every paired observation, calibration, both source
and executable captures, dependency parity, native validation identity and
remote corpus availability. Every verdict was independently recomputed before
export; all 873 third-party lock blocks also match as a sorted list, preserving
duplicates. The fixed extension remains separate until its full counts finish.

## Completed extension and final aligned-copy precision plan

Every planned extension completed. Retained-data pooling now passes all 46
original font controls. Mixed-supplementary extraction passes with 277 pairs:
+4.02%, 95% +3.70..+4.53%. Diagnostics pass ten of eleven cases. Thus the
current Linux total is **56 pass, one inconclusive, no classified regressions**.

The only remaining case is copy_aligned: +4.52%, 95% +2.45..+6.42%, retaining
all 85 pairs. Its observed log-ratio standard deviation is 0.0518907. Applying
the same planning formula with a 0.5% target half-width suggests 653 total
pairs. Subtracting 85 leaves 568 additional pairs, rounded up to 1,024.

Before running again, fix exactly **1,024 additional AB/BA pairs** for that
single case, under the unchanged ten-sample, 0.1 s warmup, 0.25 s measurement,
default-scheduler protocol. This is a separately planned precision stage after
the preceding experiment completed, not an optional extension of a running
experiment. Do not stop early or add observations based on interim estimates.
Retain all original reports and pool the first 21, the 64-pair extension and
the new 1,024 pairs: 1,109 pairs total for copy_aligned. Other case results
remain unchanged. No guaranteed pass is inferred from this precision estimate.

Output: `font-linux-recheck-20261009/aligned-precision.json`, followed
by `diagnostics-final-pooled.json`. The accepted 46-case Linux font report
remains `fonts-pooled.json`; the macOS record remains separate and unchanged.

## Final Linux result and retained evidence

The fixed final stage completed every planned pair. Copy_aligned passes with
all **1,109 pairs retained**: **+3.30%, 95% +2.89..+3.68%**. The final diagnostic
inventory passes 11/11, and the original-font inventory passes 46/46. There are
no inconclusive cases or classified regressions in this independent Linux
comparison. No production code changed during testing.

The remote audit verifies both source fingerprints, all four archived
executable hashes, the unchanged old checkout, and **3,010 pairs including
calibration: 6,020 raw Criterion sample files / 60,200 individual samples**.
It archives the raw directories, executable files, logs, plans and manifests
without including the user's private PDF contents or the build cache.

Archive: `font-linux-recheck-20261009/linux-evidence.tar.gz`,
18,735,835 bytes, SHA-256
`82937e19b492a2dcd268179fa6fd7c7d1f8234f70d4bdab0c850af196e909027`.
The remote copy remains under the same suffix in the original checkout's
target directory; a verified local mirror preserves it independently.

The [complete Linux evidence](evidence/linux-recheck-final-pairs.json) and
[complete case table](LINUX_MEASUREMENTS.md) retain every verdict and pair,
including all earlier inconclusive results. The first-run export is preserved.
The old macOS diagnostic remains a recorded regression; Linux's pass is not
an edit to that dataset or a claim that all 136 cases ran on this container.

The initial control capture then stopped before compilation because its old
workspace dependency declarations do not match the current lock's workspace
records. `cargo update --offline --workspace` corrected those records only.
All **873 third-party package blocks**, including versions, checksums and
dependency records, remain byte-identical. The resulting control lock SHA-256
is `ab2ad856223e787aded67906f4ad9bf95f32005cbd0e2d932041a9af22eb0ef5`;
current retains `c87db93c73d6e6df6f0118ab11f873f2d695db66cc264c4f6414693331644157`.
This is also the existing local reference worktree's lock identity. Neither
tree's source fingerprint changed. The failed attempt and original lock are
retained. The campaign resumed without repeating successful validation.

Control captures subsequently passed their common correctness checks and
produced the matching 11- and 46-case inventories. A read-only check of the
old remote checkout found 1/19 historical manifest files with matching hashes;
they were not automatically attached to these isolated native validation runs.

Local preparation and environment records are retained under
`font-linux-recheck-20261009/`; the remote path uses the same suffix
under the old checkout's target directory. This recheck is in progress and
has not yet produced an acceptance result.
