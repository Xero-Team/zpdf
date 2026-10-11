# Font campaign history

This file preserves earlier observations in their original order. Running and
pending descriptions below are historical. See STATUS.md for current state.

Tracking issue: https://github.com/Xero-Team/zpdf/issues/46

## Work packages

| Issue | Implementation | Acceptance |
| --- | --- | --- |
| #47 | Portable 46-case font suite, deterministic fixtures, CI checks, strict snapshots and paired comparisons implemented | Correctness/lint/smoke gates pass; host timing calibration is inconclusive |
| #48 | Memory-font provider, bounded registration, aliases, TTC sharing, document/Form propagation and generation invalidation implemented | Eleven provider tests pass; eight dedicated cases measured; full retained controls: 26 pass, 20 inconclusive |
| #49 | Bounded lazy raw/zlib packs, explicit host acquisition/cache, native/WASM examples and real CJK fixture implemented | Eight pack tests, two CJK CPU/SVG tests, six Python cache tests, four browser-helper tests and WASM target check pass; fourteen dedicated cases measured; original controls: 37 pass, 9 inconclusive |
| #50 | Automatic mixed-face page runs, union/empty clipping and multilingual TTF/TTC writer integration implemented | Integration/lint/WASM and 88 smoke cases pass; earlier page version passed all 46 controls; latest provider/original controls running |
| #51 | Complete supported CJK mappings | Not implemented |
| #52 | Measured cache improvements and final integration validation | Not implemented |

## Completed checks

- Generated coverage fonts reproduce byte-for-byte.
- The original 46-case font target passes release smoke execution.
- All eight provider cases pass release smoke execution.
- Font/document/content/benchmark test suites pass, including the eleven new provider tests.
- Affected-crate clippy with all targets and warnings denied passes.
- The comparison tools have ten passing tests for missing evidence, incompatible inputs, regressions, uncertainty and calibration.

## Measurement evidence

Host: Apple M5, macOS 26.6 arm64, Rust 1.98.1. The 19 historical private PDF fixtures are absent (0/19 present). These measurements use the portable font corpus; they are not historical full-corpus results.

| Unchanged-code protocol | Complete cases per run | Cases slower by more than 5% | Conclusion |
| --- | --- | --- | --- |
| default | 46 | 15 | Unchanged-code drift prevents accepting these runs as a stable regression gate |
| background | 46 | 1 | Unchanged-code drift prevents accepting these runs as a stable regression gate |

The two default runs have identical source fingerprints, workload hashes and toolchains. The two background runs also have identical archived executable hashes. Background scheduling applies only to the benchmark process and does not represent foreground application latency.

Retained snapshots (workspace-relative, under the ignored build directory):

- font-before-01.json
- font-before-02.json
- font-background-before-01.json
- font-background-before-02.json
- font-paired-diagnostic.json
- font-paired-diagnostic-21.json

Seven- and twenty-one-round paired diagnostics on two unchanged cases remained inconclusive at the strict 5% calibration gate. Their subsets cannot establish full-suite acceptance. No threshold was relaxed and no regression case was removed.

The performance acceptance requirement remains open. Production feature work is not considered fully accepted merely because functional tests pass. An optional question about an idle measurement host has been sent to the user; implementation can continue while final acceptance remains pending.

## Working state

Branch: codex/font-fallback-performance, based on main at 137ab2b. The pre-existing JBIG2 fix remains on its original branch. The campaign is committed on this branch and published as a pull request against main. New comments, issue text and documentation are English.

## Constructor investigation

The first provider control run flagged three of 46 cases. Twenty-one-round
paired diagnostics rejected the Latin extraction and CJK total outliers as
regressions (their paired changes were within 1%), but confirmed a roughly 12%
combined TrueType construction slowdown. An independent reference worktree at
137ab2b contains only the new benchmark harness and fixtures, with production
code unchanged.

A nine-case diagnostic suite separates input copying, parsing, owned-input
construction, metric initialization, destruction and encrypted Type1 loading.
The retained production adjustment moves Type1/malformed-font construction
into a cold, non-inlined helper. Paired owned-input construction improved to
+0.02% with a 95% interval of [-2.86%, +2.07%]; Type1 construction passed at
-1.44% with [-3.16%, +2.24%]. One destruction microbenchmark still reported
+6.91%, and one parsing interval was inconclusive. These diagnostics do not
establish full acceptance.

Lazy metric-map experiments were not beneficial for the complete operation
and have been reverted. CidWidths retains its original HashMap representation.
Three constructor regression tests cover encrypted Type1 outlines, malformed
program widths, and absent/zero/vertical metrics.

## Active validation and next actions

The retained provider implementation completed all 46 original font controls,
with 21 alternating old/new pairs per case. The report is
font-provider-retained-full-paired.json: 26 pass, 20 inconclusive, no
case classified as a regression. This is not a performance acceptance pass.
The TrueType construction control passed at +0.10%, upper 95% bound +4.90%.

The reference worktree is a task-local managed checkout (font-reference).

Keep its ignored benchmark snapshots and executables until copied elsewhere;
archive the managed worktree only after preserving required evidence.

Pack-enabled controls completed all 46 original cases with 21 alternating
pairs each: 37 pass, 9 inconclusive, no classified regressions. The report is
font-packs-controls-paired.json and the executable snapshot is
font-packs-controls-capture.json. Acceptance remains open. No case was
dropped or threshold relaxed to obtain a pass.

The current source also contains the first #50 corrections: unmapped simple
codes no longer become unrelated GIDs, absent substitute CID maps stay empty,
multi-scalar mappings do not draw just the first scalar, and missing Type0
fonts retain code segmentation and PDF metrics. These changes are newer than
the pack control executable. Execution handle 80690 completed their checks
after the pack measurements to avoid build noise.
Five added regressions, the full affected test suites, clippy and all 68
original/provider/pack smoke cases now pass. Their full performance comparison
completed: font-missing-foundations-capture.json and
font-missing-foundations-paired.json. Of all 46 cases, 45 passed and
TrueType construction was inconclusive (+1.76%, 95% upper bound +11.18%). No
case was classified as a regression. This remains an unaccepted full gate.

Newer than that capture, the provider now exposes `FontCharacter`,
`RegisteredGlyph` and `resolve_character`. It checks real cmap coverage,
supports explicit variation selectors, prefers registered bold/italic styles,
uses lazy-pack coverage hints and bounds positive/negative decisions to 4,096
entries. Registration clears cached decisions and generation checks prevent
retaining racing misses. This API performs no installed-font scanning or I/O.
Automatic page run splitting now uses this API; see the current work below.

Six integration tests, an internal cache-bound test, all font tests, affected
clippy and the wasm32 target check pass. The new 776-byte geometric UVS font
is generated without external dependencies; all prior geometric fixtures are
unchanged. All eight dedicated `font_coverage` cases completed; their original
46-case controls completed with 28 pass and 18 inconclusive. Reports:
font-coverage-initial-01.json, font-coverage-controls-capture.json,
and font-coverage-controls-paired.json. These precede page integration.

Coverage API initial medians on the geometric fixtures: cold hit 3.067 us,
warm hit 0.114 us, cold miss 2.850 us, warm miss 0.100 us, warm variation lookup
0.124 us, 256 mixed requests 30.884 us, late registration 10.667 us, lazy-pack
selection 76.344 us. These provider-level measurements do not establish
automatic page fallback performance.

## Current page and writer integration

The interpreter now preserves original metrics/extraction while substituting
actual drawing IDs and splitting adjacent faces. It handles horizontal and
vertical placement, transforms, text rise, TJ/spacing, Forms, and late
registration into an existing page cache. Page decisions are bounded to 4,096
entries and drawing programs respect the font-cache byte limit. Font resource
aliases cannot collide with internal drawing identities. Original empty glyphs
are retained; damaged sfnt/CFF outlines can use a healthy registered font.
Ambiguous multi-scalar and PUA mappings are not guessed. Invisible text skips
drawing-face admission.

Text clipping and patterned text combine all drawing faces into one outline
union. Five page tests and five resolution tests pass. The CPU clip-only fill
is pixel-identical to ordinary text drawing; SVG contains all nine visible
contours per fixture row. GPU transforms now carry positioned glyph.y for
outline and Type3 paths. Horizontal/vertical placement and ink tests passed
on Apple M5/Metal with MSAA x4. These are placement checks, not pixel-exact
antialiasing equivalence. The actual WASM target builds.

All nine initial page-fallback benchmark cases completed. All 46 original
controls passed with 21 alternating pairs per case. The largest upper 95%
bound was +1.53% (CFF outlining), below the +5% gate. Reports:
font-page-fallback-initial-01.json,
font-page-fallback-controls-capture.json, and
font-page-fallback-controls-paired.json. The completed archived binary
predates the new writer, explicit-encoding and empty-clip changes. It does not
establish performance acceptance for those later sources.

The writer now has a scalar `add_text_with_fonts` path and a TTC
face-to-sfnt embed helper. It selects shared provider coverage, embeds used
faces, and positions runs using emitted rounded PDF widths. Unsupported
scalars/selectors are explicitly rejected before page mutation. Writer
round-trip tests and the full writer suite pass for TTF and selected TTC
faces. All three writer benchmark cases completed. Median times: 64 mixed
scalars 1.953 ms, 64 single-face scalars 0.988 ms, repeated miss 1.968 us.
Final performance acceptance and broader GPU throughput checks remain open.

Execution handle 34066 finished with a missing normal dependency; adding
zpdf-font as a writer dependency resolved it. Handle 59470 completed writer
tests/lint and measurements: font-writer-fallback-initial-01.json.
The first full integration run exposed an incorrect old test that permitted
WinAnsi yen to draw the MacRoman bullet; the test now distinguishes those
glyphs correctly. Handle 25392 then completed 580 affected font/document/
content/CPU/SVG tests, eight page tests including real-GPU checks, two writer
round trips, 55 GPU unit tests, wasm32 compilation, clippy and all 88 font
smoke cases. Logs: font-integration-tests-v2.log,
font-gpu-unit-tests.log and font-integration-smoke.log.
New explicit-encoding corrections
also prevent a declared missing glyph from falling through to unrelated
Latin-1/MacRoman codes. Empty clips now block paint in CPU and SVG as they
already do in GPU. The translucent pattern test matches a single explicit
union clip; space-only clips block later painting and restore after Q on CPU,
GPU and SVG.

Latest performance validation runs in handle 50020. It captures the current
provider and original control binaries, compares eight provider cases against
the initial provider snapshot with its original default scheduler, then all
46 original cases against the reference with background scheduling. Each
uses 21 alternating pairs and retains its own failures/uncertainty. Reports:
font-integrated-provider-paired.json and
font-integrated-controls-paired.json. Captures use matching stems ending
in -capture.json. Poll the live handle before considering a restart.

The eight provider controls completed: five pass and three confirmed
regressions. Late registration measured +12.59% (95% interval +11.72..+13.08%),
registered cold page loading +10.69% (+9.92..+11.35%), and registered warm
loading +13.96% (+13.28..+14.52%). This blocks acceptance. The original 46
controls continue in the same handle; no measurements were discarded.

A follow-up optimization is drafted in the working tree: warm resolution
does not clone an Arc per glyph, simple text avoids an extra per-character
font lookup, and adjacent repeated codes reuse their drawing decision within
the text operation. It preserves generation checks between operations and
has not yet been validated. A queued run waits for the active 46-case report,
then tests/lints and compares all eight provider cases again (handle 24389).
Poll that handle before restarting. Expected report:
font-resolution-fast-provider-paired.json. Current sources are newer
than the integrated control captures.

For #51, three small authoritative Adobe resources are staged under the
ignored build directory:
adobe-GBK2K-H, adobe-HKscs-B5-H and adobe-cmap-LICENSE.md, pinned to upstream
commit f5cf3bca7fdfeaceb77aa82847e974f2306c20b4. No production CMap tables have
been replaced yet. GBK2K confirms a four-byte codespace and HKSCS its distinct
code-to-CID mapping. The upstream license allows redistribution with notices.

The follow-up preparation now retains 91 byte-verified source files in
crates/zpdf-font/data/adobe-cmaps/sources.zip (1,473,153 bytes), with a manifest
of revisions, paths, Git blobs and SHA-256 values, and both license notices.
Canonical CID-to-Unicode resources come from mapping-resources-pdf revision
2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5. Six generator tests pass for overlays,
byte lengths, four-byte codes, surrogate/ligature strings and malformed input.
Runtime tables are unchanged. Offline generation and reproducibility checking
wait in handle 27295 for the fast-provider report to avoid timing interference.

An optional six-case font_gpu target and adapter-fingerprint-aware snapshot/
paired validation are drafted for #52. They are not compiled or measured yet.
The eleven benchmark-tool tests pass, including missing/changed GPU identity
rejection. Every GPU observation will verify the adapter used for that process;
an absent adapter is an error, not a skipped timing pass.

## Font-pack evidence

The provider reserves decoded capacity before lazy decoding, including failed
face admission, and independently bounds encoded storage. It shares concurrent
decode/admission and preserves later valid candidates after an invalid pack.
Raw fonts share their input bytes. Truncated, trailing, corrupt and oversized
zlib streams fail deterministically. Runtime dependencies remain pure Rust.

The Noto Sans SC 2.004 fixture is 2,568 bytes and covers only six code points.
Its source URL, SHA-256, copyright and OFL license are checked in. Headless
tests verify all four visible Chinese characters, text extraction, original
PDF widths/positions, CPU ink and all twenty SVG contours. Native and browser
host helpers support explicit sources, digest verification and offline cache
reuse. The actual wasm32-unknown-unknown target builds successfully.

Initial release benchmark: font-packs-initial-01.json, all 14 cases,
same Apple M5 host with background scheduling. Selected medians:

| Operation | Median |
| --- | --- |
| Cold zlib decode of the tiny CJK fixture | 17.285 us |
| Cached decode | 0.006 us |
| Lazy registration | 1.038 us |
| Cold selection including decode/admission | 23.721 us |
| Warm selection | 0.434 us |
| Cold CJK page setup | 70.892 us |
| Warm CJK page setup | 6.894 us |
| Four-glyph CPU rendering at 144 DPI | 156.475 us |

These are initial new-workload measurements, not before/after regression
claims. A separate live acquisition of that fixture took 0.618 s for download,
verification and compression; verified offline reuse took 0.344 ms. The cache
was 1,687 bytes. Raw evidence is in
font-acquisition-fk2_x6ld/measurement.json. The full upstream font
download timed out; neither the tiny fixture nor the network observation can
establish full-font acquisition performance.

Next: finish pack-enabled controls and resolve performance uncertainty, then
implement #50, #51 and #52 in order. Do not mark the overall goal complete or
performance-accepted on the strength of partial diagnostics.
