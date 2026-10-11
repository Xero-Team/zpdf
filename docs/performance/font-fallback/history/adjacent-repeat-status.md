# Font campaign status

Tracking: https://github.com/Xero-Team/zpdf/issues/46

All six work packages have implementations. **Full performance acceptance is
incomplete.** The current frozen candidate adds adjacent-code reuse to the
shared-registry design. Only measurements of this exact source count toward
its acceptance. Historical candidate states are retained in
[the preceding status snapshot](history/shared-registry-status.md),
[HISTORY.md](HISTORY.md) and [CANDIDATES.md](CANDIDATES.md).

Branch: `codex/font-fallback-performance`, based on `137ab2b`.
No commits, PRs or issue closures have been made.

## Current implementation

| Issue | Implemented behavior |
| --- | --- |
| #47 | 136 portable controls, deterministic fixtures, strict snapshots, paired comparisons, interruption recovery and CI |
| #48 | Bounded Arc-backed TTF/OTF/TTC provider, aliases, stable identities/generations, document/Form/WASM propagation |
| #49 | Bounded once-only raw/zlib packs, explicit HTTPS acquisition, digest verification and offline cache |
| #50 | Coverage fallback, original PDF placement, mixed drawing faces, union/empty clipping, scalar multilingual writer and TTC extraction |
| #51 | 84 Adobe CMaps/five collections; distinct code/CID/GID namespaces; GBK2K, HKSCS, UTF-16, canonical strings, W/W2 and CFF charset handling |
| #52 | GPU atlas before outlines, lazy canonical CID recovery, narrow scalar/variation queries and bounded original-glyph checks |

The current candidate checks existing page decisions before shared original
glyph checks. Original checks use the registry's existing lock, lazy storage,
Weak LoadedFont identities and bounds of 4,096 glyphs/1,024 font identities.
They retain no font program bytes. Character coverage remains directly stored.
Composite loading resolves FontDescriptor/CIDSystemInfo once and page font
maps reserve for actual resources. Scalar coverage misses avoid full Face
construction; selected hits still validate outline usability.

Adjacent equal source identities reuse original-glyph validation within a text
operation. Simple-font decoding reuses a consecutive byte's GID and advance,
while applying positioning, character/word spacing and transforms to every
glyph. Variation-selector identities remain distinct. This avoids repeated
mapping work without changing benchmark input text.

## Current correctness evidence

The exact candidate reports **846 passes across 46 suites**, including the
synthetic performance-counter gate, actual GPU checks and doctests. Affected
all-targets clippy with warnings denied and actual wasm32 compilation pass:

- `font-repeat-final-tests.log`
- `font-repeat-final-clippy.log`
- `font-repeat-final-wasm.log`

Eight CMap generator tests, host download/cache tests, four browser-helper
tests and 20 measurement-tool tests passed in
`font-acceptance-tool-tests.log`; their implementation is unchanged.
The source oracle covers all 1,190,674 encoding entries and 113,496 canonical
strings. Historical private-corpus early returns do not establish coverage.

## Current performance evidence

Frozen source fingerprint:
`a4368230a3d94b1a50f2b7ff3f2f4060dcbc77bf44528feac35786bf436f7be8`.

Every capture below has this source fingerprint. Inputs, host and inventories
match each suite's original baseline. Paired runs explicitly use the default
scheduler for both archived executables, 21 alternating AB/BA pairs, ten
samples, 0.1 s warmup and at least 0.25 s measurement. The upper paired
bootstrap 95% bound must be at most +5%; uncertainty is not a pass.

| Suite | Cases | Current state |
| --- | --- | --- |
| Provider | 8 | All pass: `font-repeated-original-provider-paired.json` |
| Packs | 14 | All pass: `font-repeat-final-packs-paired.json` |
| Coverage | 8 | All pass: `font-repeat-final-coverage-paired.json` |
| Page fallback | 9 | All pass: `font-repeat-final-fallback-paired.json` |
| CMaps | 31 | All pass after fixed confirmation: `font-repeat-final-cmaps-pooled.json` |
| Original controls | 46 | 44 pass, two inconclusive after confirmation: `font-repeat-final-controls-pooled.json` |
| GPU | 6 | All pass: `font-repeat-final-gpu-paired.json` |
| Writer fallback | 3 | Two pass, one inconclusive after confirmation: `font-repeat-final-writer-pooled.json` |
| Constructor/copy diagnostics | 11 | Seven pass, three inconclusive, one regression: `font-repeat-final-diagnostics-pooled.json` |

Remaining capture/report paths:
`font-repeat-final-<suite>-{capture,paired}.json`, with suite names
`packs`, `coverage`, `fallback`, `cmaps`, `controls`, `gpu`, `writer`
and `diagnostics`. The complete first run reports **128 pass, eight inconclusive,
zero classified regressions**. Fixed 42-pair confirmations completed for
all eight inconclusive cases, retaining the first 21 pairs. The pooled inventory
is **129 pass, six inconclusive and one classified copy diagnostic regression**.
Full acceptance remains incomplete. The confirmation
manifest is `font-repeat-confirm-summary.json`; outputs use
`font-repeat-final-<suite>-{confirmation,pooled}.json`. Atomic per-pair
checkpoints retain validated observations if interrupted.

Provider document workloads improve 64–72% against their original baseline.
These workloads contain repeated A glyphs; tests verify every glyph and its
position. Registration cold is -3.26%, duplicate +1.03%, name hit -3.46%, name
miss +0.65%. These eight passes do not establish acceptance for the other suites.

CMaps initially had one uncertain case: vertical JIS embedded rendering
-0.13%, with a 95% interval of -8.25..+7.83%. The fixed 42-pair confirmation
retained all first-run observations; its 63-pair pooled result passes at -0.13%
(95% -2.82..+2.00%). All 31 CMap cases now pass. The other uncertain cases receive
the same fixed extension and pooling. If copy diagnostics
remain uncertain, a longer-window identical-binary calibration will be a
separate investigation; different windows cannot be pooled.

Original controls retain two uncertain cases after 63 pairs: TrueType
construction +5.69% (95% +0.93..+13.76%) and unique-CJK extraction +4.34%
(+3.83..+5.14%). All 12 render-only controls, all nine whole-page
pipelines and all three original writer controls pass. Repeated-Latin extraction
improves 63.80%; static scalar queries improve about 81%.

GPU repeated Latin improves 10.16% and mixed supplementary rendering 7.55%.
All other GPU controls pass, including mirror, shear and vertical placement.
Writer fallback passes mixed and single-face authoring; repeated unsupported
text is uncertain after 63 pairs at +6.05% (95% +4.83..+7.73%). No production
case has a classified regression, but these uncertainties remain unaccepted.

The preceding shared-registry candidate passed coverage 8/8, GPU 6/6 and writer
3/3. Packs, fallback, CMaps and original controls each had one inconclusive
case. Repeated-text provider setup regressed about 10–13%, motivating adjacent
reuse. Those reports remain historical and cannot transfer to this binary.

Pure-copy diagnostics remain unresolved. An earlier candidate had a classified
copy_heap regression; identical-binary calibration was itself inconclusive
(+3.23%, 95% -12.25..+27.40%). The preceding candidate's copy timings also varied
substantially between processes. The new complete diagnostic run must be
inspected and investigated without deleting earlier observations.

Current pooled copy_heap is classified as a regression: +28.64%, with a
95% interval of +7.38..+45.78%. Construct_owned is -0.03% (-2.86..+12.81%),
copy_aligned +1.91% (-1.34..+17.80%) and copy_unaligned +0.39% (-6.10..+5.80%);
all three are inconclusive. A fixed identical-current-binary calibration with
21 pairs, 50 samples, 1 s warmup and 2 s measurement under default scheduling
also remained inconclusive: +1.37%, 95% -7.40..+16.96%. This separate protocol
is not pooled with the short-window data. Evidence:
`font-repeat-copy-long-calibration.json`. A fixed background-scheduler
same-binary calibration now tests scheduling sensitivity; its results cannot
establish default-scheduler acceptance. See
[the copy investigation](copy-diagnostic-investigation.md).

Read-only disassembly of the original and current diagnostic binaries confirms
that their shared four-copy iteration function has the same 93 instructions
after resolving relocated external calls. It calls allocation, memcpy and
deallocation, with no zpdf font call. Evidence:
`font-repeat-copy-assembly.json` and the adjacent before/after assembly
files. This narrows the investigation but does not make uncertain timings pass.

## Costs and limits

Host: Apple M5, macOS 26.6 arm64, Rust 1.98.1. GPU: Metal, MSAA x4,
timestamp queries unavailable. Historical private PDFs: **0/19 present**.

Runtime CMap data: 2,414,312 mapping bytes plus an 18,550-byte UTF-8 text pool,
shared statically. The pinned source ZIP has 91 verified files and both Adobe
licenses. These figures do not measure process RSS or WASM download size.

Downloaded fonts supply outlines and Unicode/GID coverage; PDF Encoding and
collection mappings remain necessary. The real Noto fixture contains six code
points and does not measure full CJK-font costs. Writer fallback provides
horizontal scalar authoring without shaping or bidi. Runtime font work remains
pure Rust.

The complete 136-case inventory and every retained paired observation are
exported in [MEASUREMENTS.md](MEASUREMENTS.md) and
[evidence/current-pairs.json](evidence/current-pairs.json) (200,741 bytes).
The export revalidates source/input/executable fingerprints and recomputes
every verdict. Its status remains not_accepted. Raw Criterion observations and
archived executables remain local in the ignored build-output directory.

Resolve remaining production uncertainty and diagnostic variation, then
synchronize the final English reports and issue bodies before declaring
acceptance. Preserve ignored archived binaries and raw observations in the
reference worktree.
