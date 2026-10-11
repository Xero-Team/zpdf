# Fixed copy-callsite precision extension

The first 21-block probe is complete and remains independently archived. Its
four same-binary calibrations are inconclusive; none supports adopting a
benchmark change. The unchanged source and binary identities are verified.
The current Mac boot began before that run, so the extension can retain its
observations without crossing allocator boot-key initialization.

The original full-platform gate remains in effect while the user considers
the final acceptance platform. This diagnostic continues under that scope.
It does not clear or replace the original macOS copy_heap regression.

## Sample-count planning

Use each completed calibration's larger logarithmic distance from its median
to either 95% bound, `h`. The rough planning rule is
`N = 21 * (h / target_halfwidth)^2`, rounded up to the next common power of two.
Target 0.025 log halfwidth for the prospective shared-callsite control and
0.04 for the distinct-callsite controls. These planning targets do not change
the two-sided -5..+5% calibration rule and do not guarantee a passing result.

| Condition | Observed log halfwidth | Planning halfwidth | Approximate total pairs |
| --- | ---: | ---: | ---: |
| A/shared | 0.11820 | 0.025 | 469.41 |
| A/distinct | 0.19374 | 0.04 | 492.62 |
| B/shared | 0.08483 | 0.025 | 241.81 |
| B/distinct | 0.13730 | 0.04 | 247.43 |

Fix the final count at **512 blocks**, retaining all original 21 and adding
exactly 491. Preserve all four conditions and both repetition slots in every
block. Continue the original rotation/reversal and alternating slot order at
block index 21. Use the same archived executable paths, fixture, source,
ten samples of 131,072 iterations, minimum 0.1 s warmup and default scheduler.
No parameter or threshold is tuned during measurement. Do not stop early or
extend based on the final outcome.

The planned complete inventory is 4,096 processes / 40,960 raw samples,
including the first 168 processes / 1,680 samples. The extension uses a new
output directory and does not overwrite the first-run report or archive.
Check exact source, fixture and binary hashes plus boot identity before and
after measurement. Preserve partial and failed attempts with resumable
completion markers; never infer success from a partial output file.

At completion recompute the four same-binary calibrations and the separately
reported binary-identity contrasts using every retained observation. A failed
calibration remains unsuccessful regardless of the larger count. A passing
result would still concern this isolated probe, not zpdf before/after
acceptance. Any proposed original-benchmark repair requires its own matched
implementation comparison and continued retention of the historical regression.
