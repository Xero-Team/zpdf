# Fixed copy-callsite precision results

The [preregistered extension](copy-callsite-precision-plan.md) completed all
512 blocks. It retains the complete original 21 blocks and adds exactly 491,
without stopping early or changing the probe binaries, inputs, sample windows,
condition order, scheduler or calibration threshold. The first-run report and
archive remain immutable.

The independent [audit and complete evidence](evidence/copy-callsite-precision.json)
verify 4,096 processes and 40,960 raw samples, including the initial 168
processes and 1,680 samples. Every raw median, paired ratio and bootstrap
verdict is recomputed. Every new process has a successful completion marker;
all source and original font-benchmark hashes remain unchanged. No retry or
incomplete attempt occurred.

## Same-binary calibration

Every condition has 512 paired observations. All four pass the unchanged
requirement that the entire paired 95% interval lie within -5..+5%.

| Condition | Change | 95% interval | Result |
| --- | ---: | ---: | --- |
| A / shared allocation callsite | +0.052% | -0.169..+0.267% | pass |
| A / distinct allocation callsites | -0.023% | -0.086..+0.045% | pass |
| B / shared allocation callsite | -0.066% | -0.345..+0.241% | pass |
| B / distinct allocation callsites | -0.015% | -0.074..+0.046% | pass |

The separately reported B/A probe identity contrasts are -0.035%
(95% -0.207..+0.161%) for shared callsites and -0.050%
(-0.118..+0.001%) for distinct callsites. Both builds have identical helper
instructions and different signing identities; they are not old/new zpdf
implementations. The probe's distinct allocator calls are 128 bytes apart,
whereas the original diagnostic's source/destination callsites are 5,992 bytes
apart. Their keyed allocation contexts must not be equated.

## Interpretation and remaining acceptance item

The larger retained sample establishes precise same-binary repeatability for
this probe. Both shared and distinct modes pass; it does not demonstrate an
exclusive benefit from unifying callsites. The result therefore does not
justify replacing the original control solely to obtain a passing number.

No production code, original font diagnostic or gate changed. The original
575-pair Mac copy_heap result remains +16.86% (95% +10.40..+26.20%). Its timed
loop calls allocation, memcpy and deallocation, and no font code. The prior
allocator investigation identifies a binary-dependent contribution, but these
probe builds do not establish equivalence for the original before/after pair.
Calibration is not a substitute for that comparison.

All 125 functional Mac font controls pass. Linux passes its complete 57-case
font inventory and 135-case repaired pipeline inventory under their separate
protocols. Under the published full-platform gate, the Mac copy diagnostic
still blocks completion. The user has been asked to choose the final acceptance
platform; no exception is assumed from the unanswered question. No evidence-backed
font-library change has been identified that could repair a measured loop which
never calls the font library. Changes made only to perturb allocator placement
would not be a justified font optimization.

The full self-contained archive is
`font-copy-callsite-precision-evidence.tar.gz`: 4,446,396 bytes, SHA-256
`ae1e2718889d3792af6d9c76b347da3694ae050a1720393d58e7390003f67aa8`.
It retains the original and extension records, exact binaries, source,
disassembly, signing records, runners and audit. All processes have completed.
