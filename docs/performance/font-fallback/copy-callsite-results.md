# Copy allocation-callsite results

The fixed [plan](copy-callsite-plan.md) completed all 21 blocks: 168 processes,
1,680 raw samples, with no discarded timing observation. The independently
audited [evidence export](evidence/copy-callsite.json) retains every sample,
process order, source/executable hash and calibration result. The exact Rust
[probe source](evidence/copy-callsite-source.rs.txt) is also exported.

## Verified control and limits

Both optimized binaries have identical helper instructions but different
CodeDirectory hashes. Assembly shows one allocator call in each helper:
shared at 0x100001098 and distinct at 0x100001018. Source creation, warmup and
timed copies call the expected helper; shared-mode source and destination
therefore reach the same allocator callsite. Fixture bytes and copied output
are asserted outside timing. This uses the ordinary allocator without overrides.

The first preparation attempt requested insufficient codesign verbosity to
obtain CDHash. It failed before any timed process; its script and output are
preserved. Using -dvvvv supplies the required identity field. No sample was
removed or reclassified for this preparation issue.

## Same-binary calibration

Every row retains all 21 paired observations. Calibration requires the whole
paired bootstrap 95% interval to lie inside -5..+5%.

| Binary / source allocation mode | Change | 95% interval | Result |
| --- | ---: | ---: | --- |
| A / shared callsite | +5.19% | -2.92..+18.39% | inconclusive |
| A / distinct callsites | +4.57% | -6.15..+26.92% | inconclusive |
| B / shared callsite | -2.17% | -10.12..+6.01% | inconclusive |
| B / distinct callsites | -0.78% | -13.51..+10.01% | inconclusive |

The descriptive B/A identity contrast is -0.59% (-8.33..+3.36%) with shared
callsites and -5.60% (-22.34..+1.81%) with distinct callsites. These are two
probe builds, not zpdf implementations. Their failed calibrations prevent
using those contrasts as production equivalence evidence.

## Decision

Unifying allocation callsites is **insufficient to establish repeatable copy
timing under this fixed protocol**. The proposed benchmark modification is not
adopted. This result does not identify every remaining source of variability;
no single explanation is inferred from the timing distributions alone.

The current production/source fingerprint remains
`b078a4d72637d8707f4ed0ab466be5d307a07f89b6e13892e63b2a2a581821d9`.
Both original font diagnostic executables remain byte-identical. The original
575-pair macOS copy_heap regression is unchanged at +16.86%
(95% +10.40..+26.20%). The independent Linux 57-case font and 135-case repaired
pipeline comparisons remain passing, with their separate protocols and limits.
No production code, original diagnostic or performance threshold was changed.

The full local archive is `font-copy-callsite-evidence.tar.gz`:
2,479,731 bytes, SHA-256
`491f1fc7853d8f3cb12dcccd68fd2f3d8650a31a4b1af5dbcabdd03833dc9382`.
It preserves both executable files, source, disassembly, signing output,
all raw observations, runners, the audit and the failed preparation attempt.
The experiment is complete; no benchmark process remains active.

## Separate fixed precision extension

The first-run result above is final and unchanged. A separately preregistered
[precision extension](copy-callsite-precision-plan.md) retains all 21 blocks
and adds 491 for 512 total, keeping all four conditions and the original
measurement settings. The count comes from the completed confidence widths,
not an intermediate stopping decision. Current boot identity predates the
first run. The extension is complete: all four probe calibrations pass while
retaining every original observation. See the separate
[precision results](copy-callsite-precision-results.md). This does not clear
the original font-diagnostic regression or justify adopting a benchmark change.
