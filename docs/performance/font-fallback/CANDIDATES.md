# Font candidate history

This file preserves intermediate CMap/lookup candidates and their evidence.
Running descriptions below are historical; STATUS.md holds the current state.

Tracking: https://github.com/Xero-Team/zpdf/issues/46

Branch: `codex/font-fallback-performance`, based on `137ab2b`. No commits,
PRs or issue closures. Implementation remains under validation; the complete
performance gate is not accepted yet. The first authoritative CMap layout
was rejected for measured legacy-query regressions; a direct-directory/lazy
CID candidate is now being validated. Earlier measurements are preserved in
[HISTORY.md](HISTORY.md).

Latest draft: scalar queries parse only required sfnt/cmap/metric tables instead
of constructing and copying the full outline-parser Face. Variable fonts keep
the original variation-aware path, with metric/GID queries specialized at
compile time. Five equivalence tests cover fixture codepoints, TTC indices,
variations, unsorted directories and malformed records. Static diagnostics are
76–78% faster. Both new variable-font controls pass in the complete diagnostic
run; unrelated copy/construction diagnostics still show substantial timing
uncertainty. Full 46/31/8/6 original/CMap/provider/GPU comparisons are running as
`font-lean-*-paired.json`. Earlier candidate results below do not establish
acceptance for this newer source.

## Implementation and acceptance

| Issue | Current implementation | Acceptance |
| --- | --- | --- |
| #47 | Original 46 controls, strict executable/input inventories, paired AB/BA bootstrap comparisons, deterministic fixtures and CI | Implemented; final comparisons pending |
| #48 | Bounded Arc-backed TTF/OTF/TTC provider, aliases, stable IDs and generations; page/Form/WASM propagation | Tests pass; eight provider controls passed before the latest CMap work |
| #49 | Bounded once-only raw/zlib packs, explicit HTTPS host acquisition, SHA verification and offline cache | Tests and initial 14-case benchmark pass; real Noto subset is only six code points |
| #50 | Per-glyph recovery, mixed drawing faces, original PDF positioning, union/empty text clipping, scalar multilingual writer and TTC extraction | Final provider controls: all eight pass; complete original suite pending |
| #51 | 84 authoritative Adobe CMaps and five canonical collections; reader/writer CID separation, four-byte and variation-sequence recovery | First 31-case baseline recorded; six legacy query regressions rejected; faster directory candidate pending measurement |
| #52 | GPU atlas lookup before outline extraction, retaining page-local font/GID/device-size keys and vector fallback; lazy canonical CID recovery | Final GPU controls: all six pass; CMap controls: 23 pass, eight inconclusive; original controls pending |

## Current correctness evidence

`font-cmap-final-tests.log`: 526 font/document/content/writer unit tests
plus ten CMap and writer integration tests passed. Source tests exhaustively
compare 1,190,674 encoding entries and 113,496 canonical strings to independent
digests derived from pinned source ranges. The portable PDF tests cover both
embedded and substituted fonts, distinct source/CID/GID values, original W/W2,
four-byte GBK2K, HKSCS, UTF-16 supplementary characters and known collections
without ToUnicode. CFF writer tests use a valid original font with nonidentity
CID charset; TTC tests reject overlapping payload amplification before copying.

`font-cmap-gpu-tests.log`: 55 GPU unit tests, three CMap tests and eight
page tests passed, including actual Apple M5/Metal placement, union clipping
and empty-clip restoration. The GPU renderer uses MSAA x4; timestamp queries
are unavailable. Placement/ink tests do not claim pixel-identical CPU/GPU AA.

Affected-target clippy with warnings denied and the actual
`wasm32-unknown-unknown` build pass. Seven offline-generator tests and eleven
benchmark-tool tests pass; regeneration matches checked-in bytes exactly.
Logs: `font-cmap-final-clippy.log`, `font-cmap-final-wasm.log`,
`font-cmap-generator-tests.log`, `font-cmap-benchmark-tools.log`.

## Performance evidence

Same host: Apple M5, macOS 26.6 arm64, Rust 1.98.1. Historical private corpus:
**0/19 present**. These are portable workloads, not historical-corpus results.
The strict gate requires the upper paired 95% interval to stay at or below
+5%; inconclusive cases remain unaccepted. No thresholds or workloads changed.

The retained pre-CMap interpreter optimization separates optional painting
and clipping from ordinary text decoding. All eight provider controls pass:
`font-paint-split-provider-paired.json`. Empty provider +1.69%
(upper +2.46%), late registration +3.63% (upper +4.20%), cold registered load
+2.73% (upper +3.52%), warm registered load +4.31% (upper +4.75%). This resolves
the earlier confirmed 10.7–14% regressions; it does not validate newer sources.

The matching original suite is `font-paint-split-controls-paired.json`:
41 pass and five inconclusive, no classified regression. An older page-only
implementation passed all 46 controls, but that is not a current-source pass.

Latest archived executables:

- `font-cmap-controls-capture.json`: 46 original controls.
- `font-cmap-provider-capture.json`: eight provider controls.
- `font-atlas-gpu-capture.json`: six GPU controls with actual adapter identity.

The 31-case CMap baseline is recorded in
`font-cmap-initial-01.json`. Existing code with incorrect CID semantics
cannot serve as a correctness-equivalent baseline for these new cases. Original
and provider suites remain unchanged regression controls. Compact tables alone
are not evidence of better performance.

The first CMap candidate completed eight provider controls: seven pass and
one inconclusive (`registration/name_hit`, +3.86%, upper +5.47%). All six GPU
controls passed, including Latin repeated -13.84% and mixed supplementary
-14.22%. Evidence: `font-cmap-provider-paired.json` and
`font-atlas-gpu-paired.json`, each with 21 AB/BA pairs per case.

Original controls were stopped after 19 completed cases because six legacy
query cases had confirmed +19.61% to +82.39% regressions. Every observation is
preserved in `font-cmap-controls-rejected-partial.json`; it is explicitly
aborted and not accepted. The replacement candidate uses direct directories
for one/two-byte codes and scalar lookup without constructing string results.
It also resolves collection CIDs on demand, after the initial 24-glyph Identity
substitute pipeline measured 2.071 ms from scanning every canonical CID.
Complete original, new CMap, provider and GPU comparisons remain required.

The preceding candidate's fixed extensions completed without confirmed
regressions. Pooled results retain every observation: CMaps 30 pass and one
inconclusive; original controls 38 pass and eight inconclusive. Reports are
`font-final-cmaps-pooled.json` and `font-final-controls-pooled.json`.
The remaining CJK scalar lookup had a +5.88% paired median, 95% +3.13..+9.85%.
Disassembly showed both versions used 131 instructions apart from addresses
and field offsets, including a 2.7 KiB Face copy per query. The new narrow-table
draft removes that work without introducing a blanket Face cache.

Latest final-candidate results (21 AB/BA pairs, ten samples, 0.1 s warmup and
at least 0.25 s measurement on both sides):

- `font-final-provider-paired.json`: all eight pass. Empty provider
  -0.20%, late registration +2.17%, cold load -0.86%, warm load +2.75%.
  The largest upper confidence bound is +3.76% (name miss).
- `font-final-gpu-paired.json`: all six pass. Repeated Latin -17.19%,
  mixed supplementary -12.69%, unique CJK -8.73%, vertical mixed -6.02%.
  Shear/mirror controls pass; no pixel-equivalence claim is implied.
- `font-final-cmaps-paired.json`: 23 pass, eight inconclusive, no
  classified regressions. Identity collection substitution improves 81.48%.
- Complete original controls are running. Uncertain cases will receive one
  fixed 42-pair extension with identical settings; pooled results retain all
  earlier observations. The four pooling-tool tests pass.

`font-final-correctness-v2.log` reports 828 passes across 43 suites.
Historical corpus early returns do not establish corpus coverage. This run
includes the new vmtx/VORG tests and sfnt checksum/amplification regressions.
Final clippy, WASM and generator logs are `font-final-clippy.log`,
`font-final-wasm.log` and `font-final-generator.log`.

## Data and limits

Adobe source revisions and licenses are retained in
`crates/zpdf-font/data/adobe-cmaps/`. The offline archive has 91 verified files
(1,473,153 bytes). The new directory candidate contains 2,413,284 mapping bytes and 18,550 UTF-8
pool bytes, shared through static sections without runtime decompression.
Unicode-source pages store CIDs only; sparse/dense pages and identical sections
are deduplicated. The first layout's benchmark executable grew from 6,796,368
to 9,259,552 bytes; the direct-directory candidate is 9,294,576 bytes.

Fonts supply outlines and Unicode-to-GID coverage; PDF CMaps supply the
code-to-CID and collection-to-Unicode semantics. Downloading/decompressing a
TTF can supply missing outlines but cannot replace the PDF mapping layer.
Host helpers perform explicit acquisition; rendering performs no networking.

Writer fallback remains horizontal scalar authoring without shaping or bidi.
Predefined-CMap authoring rejects unsupported characters and incompatible CFF
collections/charsets instead of emitting an invalid CIDFontType0 CIDToGIDMap.
Vertical authoring now retains upright glyphs and correct default/per-glyph
vertical advances and origins.

## Remaining work

Finish paired original/provider/GPU measurements, investigate every classified
regression and uncertainty, record new baseline medians and executable sizes,
and update the tracking issues with final evidence. Keep the reference worktree
and ignored archived executables until evidence has been preserved; use managed
archiving when cleanup is appropriate. Do not close the campaign on partial
diagnostics or absent corpus inputs.
