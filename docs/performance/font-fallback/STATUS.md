# Font campaign status

Tracking: https://github.com/Xero-Team/zpdf/issues/46

All six work packages have implementations. **Performance acceptance is
complete for the user-selected Linux platform.** The 57-case Linux font
inventory and the repaired 135-case Linux pipeline inventory both pass the
unchanged 5% gate with every raw sample audited. The current candidate moves
writer coverage validation ahead of metric calculation and embedding. The
preceding adjacent-repeat candidate's complete results are archived and are
not automatically transferred to a changed executable. The retained macOS
heap-copy diagnostic is documented as a platform-specific limitation.

Branch: `codex/font-fallback-performance`, based on `137ab2b`.
The campaign is committed on this branch and published as a pull request
against `main`, linked from the tracking issue. All issues remain open.

## Current writer change

`DocumentBuilder::add_text_with_fonts` previously parsed metrics and computed
prefix positions before discovering an unsupported suffix. It now resolves
coverage for the complete text first, then computes positions while preparing
the embedded runs. Successful text uses the same widths and positions. Errors
during layout/embedding still roll back newly admitted faces before returning.

Four focused writer tests and affected all-targets clippy pass. A new test
causes position overflow after introducing a new face, then verifies that
existing content survives and the face can be admitted again with correct
positions. The full native/GPU command reports 847 passes across 46 suites;
warnings-denied clippy and actual WASM compilation pass. Logs are
`font-writer-preflight-full-*.log`.

All nine new-source captures are complete. Exact hashes prove that packs,
coverage, page fallback and diagnostics are byte-identical to the preceding
executables, with unchanged inputs/environment/inventories. Their earlier
evidence remains applicable through the explicit bindings in
`font-writer-preflight-binary-bindings.json`; diagnostic failures are
retained too. Writer, original controls, provider, CMaps and GPU changed and
have completed fresh comparisons. Writer passes all three cases: repeated
unsupported text -50.27%, mixed authoring +2.45% (upper +3.02%) and single-face
authoring +2.01% (upper +2.79%). Report:
`font-writer-preflight-writer-paired.json`. The other changed suites and
fixed confirmations are complete. **All 125 functional font performance
controls pass.** Across all 136 cases, 135 pass and only copy_heap remains a
classified diagnostic regression. There are no inconclusive cases after the
fixed precision comparisons and retained-data pooling.
The macOS aggregate retains that single classified diagnostic regression as a
platform-specific limitation; the equivalent Linux diagnostic passes under the
selected acceptance platform. Reports retain the writer-preflight prefix. Do
not infer equivalence from an unchanged benchmark file.

Measured writer-preflight source fingerprint:
`11f20688d746c1f2ed3883588023be862d4821293372e8c3e02fc0ee5f84aa67`.
Current capture paths: `font-writer-preflight-<suite>-capture.json`.

The earlier benchmark-only pipeline budget repair has whole-source fingerprint
`cd4c6f1aa4ce74ef65b2564edc1c215a91689d185664385e0a581731ebbcc912`.
Production library sources and all nine Mac/four Linux font executables remain
unchanged, with exact hash bindings in
`evidence/benchmark-budget-bindings.json`. All prior font outcomes are retained.

The subsequent stages output-lifetime repair has current whole-source
fingerprint `b078a4d72637d8707f4ed0ab466be5d307a07f89b6e13892e63b2a2a581821d9`.
Only stages.rs and its benchmark guide change from the budget-repair source.
Fresh compilation binds all nine unchanged Mac font executables in
`font-linux-recheck-20261009/bounded-bind-mac.json`. Both Linux captures
are complete, with matched inputs and four unchanged font binaries. The exact
bindings are in `evidence/benchmark-bounded-bindings.json`. All preparation
checks and the fixed four-case calibration pass. The completed 21-pair
full-inventory comparison passes all 135 cases; its largest upper bound is
+4.11%. All 58,380 raw samples were independently verified and mirrored locally.
See [the complete table](PIPELINE_BOUNDED_MEASUREMENTS.md). Old timing
protocols and all unsuccessful outcomes remain separate.

| Current suite | Cases | Evidence state |
| --- | ---: | --- |
| Provider | 8 | Fresh comparison: all pass |
| Packs | 14 | Byte-identical executable: all pass |
| Coverage | 8 | Byte-identical executable: all pass |
| Page fallback | 9 | Byte-identical executable: all pass |
| CMaps | 31 | Fresh comparison: all pass |
| Original controls | 46 | Fresh comparison and retained-data confirmation: all pass |
| GPU | 6 | Fresh comparison: all pass |
| Writer fallback | 3 | Fresh comparison: all pass |
| Constructor/copy diagnostics | 11 | Retained-data precision comparison: ten pass, one copy regression |

The fresh provider report is `font-writer-preflight-provider-paired.json`.
Repeated-text page paths improve 65–72%; registration and name queries pass.

Original controls initially had two inconclusive cases. The fixed 42-pair
extension retains all 21 original pairs and resolves both: TrueType construction
-0.27% (95% -4.03..+3.17%) and mixed-supplementary writer throughput +2.99%
(+2.40..+3.41%). Unique-CJK extraction passes at +3.93% (upper +4.58%).
The pooled report is `font-writer-preflight-controls-pooled.json`.
GPU repeated Latin improves 16.14% and mixed supplementary rendering 11.56%.

[The current table](PRECISION_MEASUREMENTS.md) and
[machine-readable evidence](evidence/writer-preflight-precision-pairs.json) include all
136 cases, every paired observation, current source bindings, validation log
hashes and separate calibration protocols. The export is 347,438 bytes and
recomputes every verdict after checking archived executable hashes. All
benchmark/confirmation processes have completed. Standalone allocation/copy
probes further isolate the remaining diagnostics without modifying library
source or original benchmark inputs. It finds relatively stable allocation
(18–19 ns) but wide variation even for preallocated copying (231–622 ns), under
both inherited user-interactive and user-initiated QoS. This is diagnostic
evidence, not acceptance; see the copy investigation below.

## Implemented roadmap

| Issue | Implemented behavior |
| --- | --- |
| #47 | 136 portable controls, deterministic fixtures, strict captures, paired comparisons, interruption recovery and CI |
| #48 | Bounded Arc-backed TTF/OTF/TTC provider, aliases, stable identities/generations, document/Form/WASM propagation |
| #49 | Bounded once-only raw/zlib packs, explicit HTTPS acquisition, digest verification and offline cache |
| #50 | Coverage fallback, original PDF placement, mixed drawing faces, union/empty clipping, scalar multilingual writer and TTC extraction |
| #51 | 84 Adobe CMaps/five collections; distinct code/CID/GID namespaces, GBK2K, HKSCS, UTF-16, canonical strings, W/W2 and CFF charset handling |
| #52 | GPU atlas before outlines, lazy canonical CID recovery, narrow scalar/variation queries, bounded original checks and adjacent-code reuse |

Original checks share the registry lock and retain only bounded booleans and
Weak LoadedFont identities: 4,096 glyphs and 1,024 font identities. Page decisions
take priority. Scalar coverage misses avoid full Face construction; selected
hits validate actual outline usability. Composite loading shares descriptor
resolution and page font maps reserve for actual resources.

## Preceding candidate: complete retained evidence

Source fingerprint:
`a4368230a3d94b1a50f2b7ff3f2f4060dcbc77bf44528feac35786bf436f7be8`.

All 136 cases completed with 21 AB/BA pairs; all eight inconclusive cases then
received a fixed 42-pair confirmation retaining their first observations.
The pooled result was **129 pass, six inconclusive and one classified copy
diagnostic regression**. It was not accepted.

| Suite | Pass | Inconclusive | Regression |
| --- | ---: | ---: | ---: |
| Provider | 8 | 0 | 0 |
| Packs | 14 | 0 | 0 |
| Coverage | 8 | 0 | 0 |
| Page fallback | 9 | 0 | 0 |
| CMaps | 31 | 0 | 0 |
| Original controls | 44 | 2 | 0 |
| GPU | 6 | 0 | 0 |
| Writer fallback | 2 | 1 | 0 |
| Constructor/copy diagnostics | 7 | 3 | 1 |

Pending production paths were TrueType construction +5.69% (95% +0.93..+13.76%),
unique-CJK extraction +4.34% (+3.83..+5.14%) and writer repeated unsupported
text +6.05% (+4.83..+7.73%). The writer's unnecessary prefix metrics motivated
the current change.

Earlier passes include repeated-A provider setup 64–72% faster, static scalar
queries about 81% faster, Identity collection substitution 74% faster, GPU
repeated Latin 10% faster and mixed supplementary GPU rendering 8% faster.
These gains apply to their recorded portable workloads and exact binaries.

[The complete table](history/adjacent-repeat-measurements.md) and
[machine-readable evidence](evidence/adjacent-repeat-pairs.json) retain every
paired observation, input/source/executable hashes and recomputed verdicts.
The export is 200,741 bytes. Raw Criterion files and archived executables remain
in the ignored build-output directory. The preceding full command reported 846
passes across 46 suites, with lint and WASM success in
`font-repeat-final-*.log`.
See [its status snapshot](history/adjacent-repeat-status.md) for exact reports.

## Copy diagnostic investigation

The 63-pair copy_heap result was +28.64% (95% +7.38..+45.78%), classified as a
regression. The copied 21,088-byte fixture and shared 93-instruction copy loop
are unchanged; the loop calls allocation/memcpy/deallocation and no font code.
This does not make the timing result pass.

Same-binary calibration remained inconclusive with longer windows (50 samples,
1 s warmup, 2 s measurement: +1.37%, 95% -7.40..+16.96%) and with background
scheduling (+/−5% calibration: -3.42%, 95% -12.43..+3.46%). Neither experiment
established a stable measurement condition. Different protocols remain separate.
See [the investigation](copy-diagnostic-investigation.md). Do not change font
behavior merely to hide an unresolved pure-copy timing result.

## Protocol, costs and limits

Host: Apple M5, macOS 26.6 arm64, Rust 1.98.1. GPU: Metal, MSAA x4, timestamp
queries unavailable. Historical private PDFs: **0/19 present**.

Default-scheduler comparisons use ten samples, 0.1 s warmup and at least
0.25 s measurement. The upper paired bootstrap 95% bound must be at most +5%.
Inconclusive is not pass. Complete inventories, fixed inputs and every prior
observation are retained. Different scheduling/window protocols cannot be pooled.

Runtime CMaps use 2,414,312 mapping bytes and an 18,550-byte text pool. The source
ZIP contains 91 verified files and Adobe licenses. The real Noto fixture has
six code points; its costs do not represent a full CJK font. Writer fallback is
horizontal scalar authoring without shaping or bidi. Runtime font work is pure
Rust. Downloaded font outlines complement the PDF's encoding/CID mappings.

Eight generator tests, host download/cache tests, four browser-helper tests
and 20 measurement-tool tests passed; these tools are unchanged. Their evidence
is `font-acceptance-tool-tests.log`. Preserve ignored reference-worktree
binaries and raw observations. All seven English issue bodies are synchronized
with these results, and the six native sub-issue relationships remain verified
and open. The single macOS heap-copy regression remains recorded as a
platform-specific limitation. High-sample-count same-binary calibrations now
pass, but the actual macOS before/after result still exceeds the gate; it is
not relabeled as noise.

A subsequent layout probe did not find a stable destination-offset setting.
A variance-based, fixed 512-pair same-binary calibration now passes: +1.04%,
95% -0.49..+3.17%. Output: `font-copy-precision-calibration.json`.
This establishes calibration for that case, not full-suite acceptance.

The fixed comparisons are complete and retain every earlier observation:
1,024 additional pairs for construct_owned and 512 each for
copy_heap/copy_aligned/copy_unaligned. The complete diagnostic pool reports
ten passes and one regression. Construct_owned passes at -0.78% (1,087 pairs),
copy_aligned +0.15% and copy_unaligned -0.11% (575 pairs each). Copy_heap remains
a regression at +16.86%, 95% +10.40..+26.20%, with all 575 pairs retained.
Thus the current aggregate is **135 pass and one regression**, with no remaining
inconclusive case. That regression is retained as a macOS platform limitation.
Pooled evidence: `font-diagnostics-precision-pooled.json`.

A separate fixed 512-pair control now compares two fresh archives of exactly
the same current executable, with equal-length paths. It isolates file identity
from code changes and is not pooled with before/after data. See
[the preregistered plan](diagnostic-calibration-plan.md). Output:
`font-copy-path-calibration.json`. Its complete 512 pairs pass at -0.58%,
95% -3.07..+0.13%, so distinct file paths alone do not explain the regression.
Those diagnostic experiments are complete. Library source and original
workloads remained unchanged during them.

The subsequent allocator investigation identifies a concrete contribution to
the copy result. Installed libsystem_malloc instructions and 24 checked
predictions corroborate executable-specific type bucketing: the before
binary's two allocation sites can share a bucket; the current binary's cannot
under this boot's keys. A fixed single-allocation intervention preserves the
keys and all later allocation descriptors, changing only the source buffer's
bucket. Separate versus same source buckets increases copy cost by 93.71%
before (95% +66.17..+115.05%) and 71.10% after (+57.35..+87.06%).

All 96 successful launches, 960 samples and one pre-timing failed attempt are
retained; a broader earlier intervention also retains all 96 launches. These
are diagnostic protocols, never pooled with acceptance. They establish an
allocator contribution, not that the entire original difference has been
accounted for or that equivalence within 5% has been shown. Full acceptance
therefore remains **135 pass, one regression**. No library code, original
workloads or gate changed. See [the investigation](copy-diagnostic-investigation.md).

The user subsequently supplied a remote x86_64 Linux container for independent
testing. Isolated control/current sources have been transferred and verified
under the existing checkout's target directory, preserving its clean old
checkout. The current source fingerprint is identical to the frozen candidate;
the independent control uses production commit 137ab2b with matching benchmark
workloads and identical third-party dependency records (workspace-only lock
records differ). Linux native validation reports 788 passes across 42 suites.
The fixed calibration and all planned precision stages completed. The final
independent Linux comparison passes **57/57, with no inconclusive cases or
classified regressions**.
All 46 original font controls pass; mixed-supplementary extraction passes with
277 retained pairs at +4.02% (upper +4.53%). Copy_heap passes at -0.13%
(95% -1.31..+1.01%). Copy_aligned passes at +3.30%
(+2.89..+3.68%, all 1,109 pairs retained). The audit verifies unchanged source,
four executable hashes and all 60,200 raw samples including calibration.
The original remote checkout is unchanged; a verified local archive preserves
the remote binaries and raw observations. That font-only run is complete. See
[the Linux recheck](LINUX_RECHECK.md). Linux results remain separate from the
retained macOS evidence and do not rewrite its copy_heap regression.

The completion audit separately identified missing timing evidence for the
original synthetic `stages` controls, explicitly required by the font guide.
The existing perf_gate verifies counters and a loose timeout, not relative
performance. The helper also overrode explicit CLI budgets; that measurement
bug is repaired and covered by focused tests. Both sides now pass all 135
case smoke checks with matched inputs and actual requested sampling settings.
The fixed Type3 calibration and all 21 full-inventory pairs are complete:
**131 pass, four inconclusive, zero classified regressions**. All 27 parse,
27 interpret, 27 encode and 27 total cases pass. CPU rendering has 23 passes
and four uncertain cases, so the complete matrix is not accepted yet.
All 57,120 raw samples, 84 process markers and executable/input hashes were
independently verified and archived both remotely and locally. See
[the pipeline recheck](PIPELINE_RECHECK.md) and
[the complete table](PIPELINE_MEASUREMENTS.md); these remain separate from
the already complete 57-case Linux font result.

Raw-sample inspection finds that the uncertain render loop retains multiple
full RGBA outputs according to Criterion's sample iteration count. Type3 @150
also splits into two iteration plans with different median costs. A separate
fixed 21-block same-current-binary diagnostic is now complete: SmallInput has
three passing calibrations and one inconclusive calibration; PerIteration
passes all four. All 3,360 raw samples were independently recomputed and
archived, and the original source/binary identities are unchanged. This
supports investigating bounded output lifetimes in the render benchmark,
but requires fresh before/after evidence for any repaired workload. It does
not clear the four uncertain original results or the macOS copy regression.
That diagnostic is complete. The justified PerIteration repair is now applied
identically to both original stages trees. The new protocol requires fresh
four-case calibration and all 135 before/after cases. Preparation, calibration
and all 135 fresh comparisons pass, with no inconclusive cases or regressions.
The original SmallInput outcomes remain intact; protocols are never pooled.
The complete archive and independent audit cover 58,380 raw samples and all
source/input/executable identities. No pipeline measurement remains active.
The repaired pipeline inventory passes under the selected Linux acceptance
platform; the historical macOS heap-copy result is retained separately.

A fixed allocation-callsite probe now rules out adopting a simple shared-callsite
control on the basis of its current evidence: all four same-binary calibrations
remain inconclusive. Both probe builds' helper instructions match, and all 168
launches / 1,680 raw samples were independently audited. No benchmark or
production change is made from this unsuccessful probe. See
[the results](copy-callsite-results.md). The user subsequently selected the
supplied Linux environment as the final performance acceptance platform. The
historical macOS heap-copy result is retained as a documented
platform-specific limitation and is not excluded, relabeled or silently
cleared; no threshold, fixture or case was changed.

Under the unchanged full-platform scope, the fixed callsite-probe precision
extension is complete. Width-based planning selected 512 total blocks,
retaining all initial 21 and adding 491 with identical binaries, callsites,
condition order, sample settings and the same Mac boot. The independently
audited 4,096 processes / 40,960 samples pass all four same-binary calibrations.
Both shared and distinct modes pass; no exclusive benefit of the prospective
helper is established. See [the complete results](copy-callsite-precision-results.md).

This is not an old/new zpdf comparison and cannot clear the original +16.86%
result, which remains recorded. All planned experiments are complete and no
benchmark remains active. Under the selected Linux acceptance platform, both
the font and pipeline inventories pass their separate complete protocols. No
font-library change is justified merely to alter the allocator placement of a
timed control which calls no font code.

The final English bodies for #46-#52 carry this precision result and the
Linux acceptance decision. The user authorized the GitHub synchronization with
the measured performance data and without local paths. Earlier issue updates
remain published and verified, and the native #47-#52 relationships and open
states remain verified.
