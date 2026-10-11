# Font campaign status

Tracking: https://github.com/Xero-Team/zpdf/issues/46

All six work packages have implementations. Full performance acceptance is
still pending. The current candidate shares the registry's existing lock for
lazy original-glyph checks, with direct character-coverage access and per-page
decision priority. Earlier candidates below are retained history, not acceptance
for changed code.
Branch: `codex/font-fallback-performance`, based on `137ab2b`.
No commits, PRs or issue closures. Previous evidence is preserved in
[HISTORY.md](HISTORY.md) and [CANDIDATES.md](CANDIDATES.md).

## Current implementation

| Issue | Implemented behavior |
| --- | --- |
| #47 | Original 46 controls, deterministic fixtures, strict snapshots, complete inventories, paired comparisons and CI |
| #48 | Bounded shared TTF/OTF/TTC memory provider, aliases, stable IDs/generations, document/Form/WASM propagation |
| #49 | Bounded once-only raw/zlib packs, explicit HTTPS host acquisition, digest verification and offline cache |
| #50 | Coverage fallback, original PDF placement, mixed drawing faces, union/empty clipping, scalar multilingual writer and TTC extraction |
| #51 | 84 Adobe CMaps/five collections; correct code/CID/GID namespaces, GBK2K/HKSCS/UTF-16, canonical strings, reader/writer W/W2 and CFF charset handling |
| #52 | GPU atlas before outlines, lazy canonical CID recovery, narrow scalar/variation queries without a blanket Face cache |

The latest scalar query reads the table directory once. Static queries parse
only required/cmap/metric tables. Variable advances use ttf-parser's fvar/avar,
HVAR and gvar machinery via Face::from_raw_tables, with all required dependencies.
GID-only scalar queries do not compute metrics. See
[scalar-lookup-design.md](scalar-lookup-design.md).

## Correctness

The current complete acceptance command reports 844 passes across 46 suites,
including the existing synthetic performance-counter gate, actual GPU checks,
and doctests. Clippy with warnings denied and the actual wasm32 target build
pass. Generator, host download/cache and browser helper tests pass; the
measurement tools have 20 passing tests. Current logs are
`font-acceptance-{tests,clippy,wasm,tool-tests}.log`. The source oracle
still verifies every mapping. Private-corpus early returns are not coverage.

Current v5 validation reports 840 passes across 45 suites, including original
glyph-check weak ownership/bounds, descriptor axes for inline and indirect
composite fonts, source-oracle coverage and actual GPU tests. Warnings-denied
clippy and wasm32-unknown-unknown compilation pass. Eight CMap generator tests
and byte-identical regeneration pass. Evidence: `font-final-v5-tests.log`,
`font-final-v5-clippy.log`, `font-final-v5-wasm.log`, and
`font-final-v5-generator.log`. Corpus early returns remain excluded from
historical-coverage claims.

The preceding complete run reported 833 passes across 44 suites. The latest
single-pass candidate passes the full font/document/content/writer/CPU/SVG and
actual-GPU commands, warnings-denied clippy, and wasm32-unknown-unknown build.
Logs: `font-final-v4-tests.log`, `font-final-v4-clippy.log`,
`font-final-v4-wasm.log`. Historical corpus tests may return early;
reported pass counts do not establish coverage of absent private files.

The source oracle covers all 1,190,674 encoding entries and 113,496 canonical
strings. Seven generator tests pass and regeneration is byte-identical. Five
scalar-query equivalence tests cover fixture codepoints, variable instances,
TTC indices, unsorted directories and malformed/duplicate records. Twenty
measurement-tool tests cover incomplete evidence, protocol mismatch,
regressions, uncertainty, pooling, scheduler overrides and interruption recovery.

## Performance gate

Same host: Apple M5, macOS 26.6 arm64, Rust 1.98.1. GPU: Metal, MSAA x4,
timestamp queries unavailable. Historical private PDFs: **0/19 present**.
Portable data and workload hashes remain fixed. An upper paired bootstrap 95%
bound above +5% cannot pass; uncertainty is not a pass.

The first CMap layout was rejected for 19.61–82.39% legacy-query regressions.
Direct one/two-byte directories removed those regressions. Earlier candidates
showed 66–76% faster legacy decoding, 76–79% faster static scalar queries,
roughly 79–81% faster Identity collection substitution, and about 14–17% faster
repeated Latin/mixed supplementary GPU rendering. These do not transfer
acceptance to a changed binary.

The first scalar fallback prototype made default variable queries about 5.7%
slower. The current single-pass version passes both dedicated controls:
configured axes +1.71% (upper +2.12%), default instance +2.07% (upper +2.30%).
Source: `font-lookup-v4-variable-diagnostic.json`. It is a diagnostic
subset, not full-suite acceptance.

Background-scheduler runs showed wide uncertainty, even for unchanged work.
Identical-binary calibration passed for both schedulers, with a narrower
default-scheduler interval: -0.73..+0.59% versus -0.34..+1.79% in background.
Neither calibration proves that all previous uncertain cases were noise.
Reports: `font-scheduler-calibration-{default,background}.json`.

The current complete run explicitly uses default scheduling for both archived
binaries, 21 alternating pairs, ten samples, 0.1 s warmup and at least 0.25 s
measurement windows. Original snapshot metadata is retained. No thresholds or
inputs changed; different scheduling protocols cannot be pooled. Results:

| Current report | Cases | State |
| --- | --- | --- |
| `font-final-v4-diagnostics-pooled.json` | 11 | Seven pass, three inconclusive, one copy diagnostic classified as a regression |
| `font-final-v4-controls-paired.json` | 46 | 44 pass, two inconclusive; fixed confirmation running |
| `font-final-v4-cmaps-paired.json` | 31 | 30 pass, one inconclusive; fixed confirmation queued |
| `font-final-v4-provider-paired.json` | 8 | All pass |
| `font-final-v4-gpu-paired.json` | 6 | All pass |

The interrupted original controls were resumed after confirming the process
had stopped. All 462 validated pairs from the first 22 cases were retained.
New atomic checkpoints preserve each complete pair, and completion markers
support recovery after interruption. Seventeen legacy attempts without process
completion evidence remain archived as unverified; they were not treated as
successful timings.

The copy_heap diagnostic remains unaccepted after a fixed 42-pair confirmation.
An identical-binary 21-pair calibration is also inconclusive: +3.23%, with a
95% interval of -12.25..+27.40%. This does not clear the regression, but it shows
that the allocation/copy diagnostic needs further measurement investigation.
Reports: `font-final-v4-copy-calibration.json` and
`font-final-v4-diagnostics-{confirmation,pooled}.json`.

Final pack (14), coverage (8), page fallback (9) and writer fallback (3) binaries
are captured and their input hashes match their original baselines. Complete
comparisons are queued after the fixed confirmations. Active execution handle
at this update: 58050; revalidate it before resuming work. Expected reports are
`font-final-v4-{packs,coverage,fallback,writer}-paired.json`.

## Latest performance investigation

The v4 original controls pass all 46 cases after pooling the fixed confirmation:
`font-final-v4-controls-pooled.json`. Provider 8/8, GPU 6/6 and writer
3/3 also pass. CMaps remain 30/31, with four-byte GBK decoding uncertain.

Final pack/coverage/fallback comparisons exposed additional regressions that
the original suite did not exercise: real CJK cold/late setup +14–16%, warm
setup +89.56%, cold coverage hits/misses +18.36%/+8.82%, late registration
+6.84%, and vertical page fallback +5.45%. These are preserved in the v4
reports and prevent accepting that candidate.

V5 addresses measured duplicate work:

- CID/Unicode mapping no longer probes the same outline before drawing checks.
- Original-glyph usability is cached by immutable LoadedFont identity using
  Weak references, so font programs can be freed. Bounds are 4,096 glyph checks
  and 1,024 font identities, including empty/missing fonts. A text operation
  shares one parser and lock; usable originals need no page fallback map.
- Coverage misses parse only cmap metadata; full outline validation remains
  mandatory for selected hits and damaged-glyph recovery.
- Composite font loading resolves CIDSystemInfo and FontDescriptor once,
  sharing descriptor data across program lookup, hints and variation axes.
- Page font maps reserve for actual resources (capped at the prior 256-entry
  hint), retaining dynamic growth and stable IDs for nested Forms/fallbacks.
- A 1,028-byte high-byte directory narrows the long GBK2K prefix search.

Focused diagnostics show cold coverage hit -9.22%, cold miss -32.51% and late
registration +0.90%. The latest real-CJK cold/late diagnostics pass at -1.30%
and -1.61%; warm setup is -2.09% but still inconclusive (upper +5.47%). These
subsets cannot establish a full-suite pass.

All nine v5 suites were captured for sequential comparison with the same strict
default-scheduler protocol: packs 14, coverage 8, fallback 9, CMaps 31, original
46, provider 8, GPU 6, writer 3, diagnostics 11 (136 cases total). Report paths
are `font-final-v5-<suite>-paired.json`; per-pair checkpoints support
resumption. The run was stopped after confirmed mixed-fallback regressions;
`font-final-v5-cmaps-partial.json` retains four partial case records.

V5 coverage passes all eight cases. All real-CJK pack setup cases pass: cold
-1.78%, late -1.33%, warm +2.90% (upper +4.13%). Three other pack cases remain
inconclusive. However, putting shared checks before page decisions regressed
mixed fallback: cold +6.38%, warm +11.09%, extraction +10.56%, Form +6.74%, and
warm page-cache interpretation +16.66%. V5 is not accepted.

The current correction checks per-page decisions first, retains successful
original identities in an already allocated page cache, and skips text-wide
prechecking when a mixed-run cache already exists. Focused correctness and
warnings-denied clippy pass. Complete fallback (9) and pack (14) comparisons
are active: `font-cache-order-{fallback,packs}-paired.json`.
Execution handle at this update: 77169; revalidate it before resuming work.

That cache-order candidate completed all nine page-fallback cases with a pass.
Its pack confirmation still showed a lazy-registration regression (+8.64%,
95% +6.52..+9.91%). Boxing only the outline cache was insufficient; boxing the
character cache then regressed warm variation selection (+6.84%). Those drafts
were rejected. Character coverage remains directly accessible, and the lazy
original-check map now uses the existing registry lock with one optional pointer.

The current shared-lock candidate passes all eight coverage cases: cold hit
-6.90%, cold miss -36.94%, late registration -1.26%, warm variation -2.31%.
Focused real-CJK cold/late/warm and lazy registration also pass. Full current
comparisons are running for packs, fallback, CMaps, original controls, provider,
GPU, writer and diagnostics. Coverage is already complete for the same source.
Paths: `font-shared-lock-<suite>-paired.json`. Current execution handle:
86927. Check that handle and the actual reports before treating it as live.

Latest completed current-source results: coverage 8/8 pass; packs 13/14 pass,
with only cached invalid decoding inconclusive (-3.13%, 95% -5.24..+6.28%).
Every real-CJK pack setup/render case and lazy registration passes. Current
fallback completed with eight passes and one inconclusive warm page-cache
case (+4.42%, upper +5.98%). CMaps and the remaining suites are still being measured. Do not transfer
the preceding cache-order candidate's 9/9 fallback pass to this newer binary.

## Costs and limits

Runtime CMap data: 2,414,312 mapping bytes plus an 18,550-byte text pool, shared
statically without per-font copies. The pinned source ZIP has 91 verified files
and both Adobe licenses. The native font benchmark grew from 6,796,368 bytes
before CMap work to 9,318,640 bytes for v5. This is not measured RSS or WASM size.

Downloaded fonts supply outlines and Unicode/GID coverage. They complement,
rather than replace, PDF Encoding and collection mappings. The real Noto fixture
has only six code points; its acquisition/pack timings are not full-font costs.
Writer fallback is horizontal scalar authoring without shaping or bidi.

Finish current comparisons, preserve every uncertainty or regression, and
publish the final English evidence before declaring acceptance. Keep reference
worktree binaries and ignored raw observations until they have been preserved.

