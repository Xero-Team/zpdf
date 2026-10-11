# Font coverage and performance campaign

Status: all six work packages have implementations in the working tree.
Performance acceptance is complete for the user-selected Linux platform: the
57-case Linux font inventory and the repaired 135-case Linux pipeline inventory
both pass the unchanged 5% gate with every raw sample audited. The provider,
lazy raw/zlib packs, explicit native/browser acquisition, per-glyph recovery,
multilingual writer, authoritative Adobe CMaps and GPU atlas-before-outline
path are implemented. All 125 functional font controls pass on macOS; the
complete 136-case inventory retains one classified heap-copy diagnostic
regression, documented as a macOS platform-specific limitation. The separate
135-case original synthetic pipeline comparison reports 131 passes and four
uncertain CPU-render cases after repair of the helper's CLI budget precedence.
All 27 end-to-end cases pass; the repaired protocol passes the complete matrix.
A separate output-retention diagnostic supports PerIteration batching for
the render benchmark. Both sides use that explicit repair, all four same-binary
calibrations pass, and the complete fresh comparison passes all 135 cases.
See [the complete repaired-protocol table](PIPELINE_BOUNDED_MEASUREMENTS.md).
The original SmallInput results and macOS copy regression remain recorded.

See [STATUS.md](STATUS.md) for current results and limitations, and
[HISTORY.md](HISTORY.md) for prior candidates and rejected experiments.

The implementation order and acceptance criteria are recorded in [the tracking proposal](issues/00-tracking.md) and its six work packages. Current tables are in [PRECISION_MEASUREMENTS.md](PRECISION_MEASUREMENTS.md) and [LINUX_MEASUREMENTS.md](LINUX_MEASUREMENTS.md); the separate pipeline protocol is in [PIPELINE_RECHECK.md](PIPELINE_RECHECK.md).

See [the architecture audit](architecture-audit.md) for the distinction between
glyph programs, encoding tables, downloaded fallback fonts and PDF embedding.

## Regression policy

Use same-host release measurements and unchanged fixture identities. A repeatable regression above 5% in an existing workload blocks acceptance. Missing cases, missing fixtures, different hosts, and faster rendering caused by dropped text cannot establish a pass. New recovery workloads must also prove glyph identity, extraction and placement.

## Initial audit

- Base: local main at 137ab2b (workspace version 0.13.0).
- Existing targets: stages, backend, batch, and opt-in longform.
- Portable synthetic text coverage currently consists only of a Type3 fixture.
- The checked-in historical baseline is incomplete and was captured on another host.
- Dedicated font loading, coverage, fallback, pack decoding and mapping gates must be added before production changes.

## GitHub work packages

Tracking issue: https://github.com/Xero-Team/zpdf/issues/46

- [#47: bench(font): add reproducible coverage, loading and rendering baselines](https://github.com/Xero-Team/zpdf/issues/47)
- [#48: feat(font): support registered memory fonts through a shared provider](https://github.com/Xero-Team/zpdf/issues/48)
- [#49: feat(font): load bounded fallback font packs lazily](https://github.com/Xero-Team/zpdf/issues/49)
- [#50: fix(font): resolve missing glyphs by coverage while preserving PDF placement](https://github.com/Xero-Team/zpdf/issues/50)
- [#51: fix(font): complete supported CJK CMaps and encoding coverage](https://github.com/Xero-Team/zpdf/issues/51)
- [#52: perf(font): optimize measured glyph work and validate the complete fallback pipeline](https://github.com/Xero-Team/zpdf/issues/52)
