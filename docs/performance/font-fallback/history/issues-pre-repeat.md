# Issue bodies before current-source acceptance

This snapshot preserves prior implementation and measurement history. It is
not the current acceptance state; consult STATUS.md and the issue body sources.

At synchronization, remote #51 differed from this local snapshot only in an
older result count: it said 24 pass/seven inconclusive, while the local body
said 23 pass/eight inconclusive. Both describe a preceding candidate; the
current source has its own complete report and neither count is transferred.

## 00-tracking.md

# Improve font coverage and fallback with benchmark-gated implementation

## Problem or use case

zpdf reads embedded fonts and substitutes installed fonts, but substitution selects one face by name without a complete per-glyph coverage fallback chain. The renderer has no application font provider, browser deployments cannot rely on system fonts, and predefined CJK CMaps have incomplete mappings. Large generated encoding tables map character codes to Unicode; they do not contain glyph outlines and cannot be replaced by downloading a TTF.

The current benchmark suite measures pipeline stages but lacks dedicated font loading, mapping, fallback and font-pack workloads. Its checked-in baseline is machine-specific and incomplete.

## Proposed behavior

Implement the following dependent work packages in order, starting with reproducible measurement:
1. Dedicated font benchmarks, deterministic fixtures and strict comparison gates.
2. An additive font-provider API for registered in-memory and system fonts.
3. Bounded, lazily decoded font packs and optional host-managed acquisition.
4. Coverage-aware glyph fallback with positive/negative caching and preserved PDF placement.
5. Complete supported CJK encoding/CID mappings and explicit unsupported cases.
6. Benchmark-driven glyph/cache improvements and integration validation.

Preserve usable embedded glyphs. Resolve missing glyphs through reliable Unicode or collection mappings, never through arbitrary GID reuse. Keep network requests and decompression outside per-glyph rendering. Support native and WASM callers without introducing C/C++ dependencies.

## Alternatives considered

Keeping only system-font substitution leaves headless and browser environments dependent on unavailable fonts. Replacing encoding tables with TTF files does not solve code-to-Unicode or code-to-CID mapping. Unconditionally caching all glyph bitmaps is not justified by the prior performance experiments.

## Compatibility and specification impact

Prefer additive public APIs and preserve the default path for callers that do not register extra fonts. PDF character codes, CIDs, Unicode and GIDs are distinct namespaces (ISO 32000-1 sections 9.6-9.10). Preserve PDF widths, vertical metrics, explicit positioning and text extraction.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Execution

Each child issue will contain scope-specific acceptance criteria and benchmark evidence. All source comments, issue text, commit messages and PR descriptions produced for this work will use English. This technical plan is AI-assisted and was requested explicitly by the repository user.

## Linked sub-issues

All six packages now have working-tree implementations. The current release
gate remains open: strict paired controls include uncertainty, and the first
authoritative CMap directory was rejected for measured query regressions.
The replacement passed all six targeted old-encoding diagnostics (roughly
66–73% faster than the original controls); complete comparisons are running.
The first atlas candidate passed all six actual-GPU controls, with repeated
Latin and mixed supplementary workloads about 14% faster. Those results do
not substitute for final source-wide acceptance. Historical private PDFs
remain absent (0/19), and no commits, PRs or issue closures have been made.

- [#47: bench(font): add reproducible coverage, loading and rendering baselines](https://github.com/Xero-Team/zpdf/issues/47)
- [#48: feat(font): support registered memory fonts through a shared provider](https://github.com/Xero-Team/zpdf/issues/48)
- [#49: feat(font): load bounded fallback font packs lazily](https://github.com/Xero-Team/zpdf/issues/49)
- [#50: fix(font): resolve missing glyphs by coverage while preserving PDF placement](https://github.com/Xero-Team/zpdf/issues/50)
- [#51: fix(font): complete supported CJK CMaps and encoding coverage](https://github.com/Xero-Team/zpdf/issues/51)
- [#52: perf(font): optimize measured glyph work and validate the complete fallback pipeline](https://github.com/Xero-Team/zpdf/issues/52)

## 01-benchmarks.md

# bench(font): add reproducible coverage, loading and rendering baselines

## Problem or use case

The four current Criterion targets cover pipeline stages and throughput, but the portable synthetic corpus exercises Type3 text only. There is no direct benchmark for TrueType/CFF loading, simple/composite mapping, repeated versus unique glyphs, missing glyphs or font fallback. The existing Windows baseline cannot establish regressions on a different host.

## Proposed behavior

Add a dedicated fonts target with deterministic synthetic Latin/CJK/supplementary-plane fixtures, embedded-font and missing-font cases, repeated/unique text, cold construction and warm document reuse, extraction and CPU rendering controls. Include variable-font and CFF coverage. Fixtures must assert actual glyph/ink/text behavior before timing.

Add strict, scoped baseline recording/comparison with fixture and environment identity, expected case coverage, and no silent success for empty, stale or incomplete results. Preserve existing benchmark workflows. Document how later provider, pack, fallback and CMap cases extend this suite.

## Acceptance criteria

- A clean clone can run the font suite without installed fonts, private PDFs or network access.
- Cold and warm measurements measure the stated operation; setup excluded from a stage is documented.
- Correctness checks prevent dropped glyphs from appearing as speedups.
- A same-host release baseline and an unchanged rerun are recorded before production font behavior changes.
- CI checks the font fixture semantics and benchmark harness.
- Missing original private corpus files remain explicitly reported, never represented as a full historical-corpus comparison.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

The portable suite now has 46 font controls, eight provider workloads, and
nine construction diagnostics. Correctness gates check glyph IDs, PDF
placement, extraction and ink. Strict snapshots reject missing cases and
incomparable workloads; archived executables support alternating paired
measurements with pass/regression/inconclusive outcomes.

The original private corpus is absent on this host (0/19 files). The portable
suite is explicitly not represented as a historical full-corpus run. Unchanged
binary calibration exposed host timing drift, and the initial implementation
also had a reproducible constructor regression; both are being investigated
without relaxing the 5% threshold. Full performance acceptance remains pending.

## 02-provider.md

# feat(font): support registered memory fonts through a shared provider

## Problem or use case

Font loading is coupled to the process-wide system directory index. LoadedFont already accepts Arc-backed font bytes, but callers cannot consistently supply their own fonts throughout document loading and Form XObjects. The one-time directory scan cannot discover fonts downloaded after initialization.

## Proposed behavior

Introduce an additive provider API for validated in-memory TTF/OTF/TTC faces, explicit family/style metadata and the existing system source. Thread the provider through document and nested resource loading. Share immutable font bytes and use stable face identities and a provider generation for cache invalidation.

## Acceptance criteria

- Registered fonts work without filesystem or system-font access and are exposed through the facade.
- Existing APIs and embedded-font behavior remain compatible.
- TTC face indices, malformed input, duplicate registration, limits and ownership lifetimes are covered.
- Late registration invalidates previous misses.
- Benchmarks cover cold registration, warm selection, document sharing and the unchanged embedded path.

Depends on the benchmark sub-issue.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

The additive provider is implemented locally, including validated shared
TTF/OTF/TTC bytes, bounded cumulative aliases, Unicode aliases, metadata-based
style selection, late-registration invalidation, and propagation into nested
and inline Form resources. Eleven provider regression tests and the existing
affected suites pass. Eight dedicated release workloads have been measured.

The first 46-case control run flagged three cases. Paired diagnostics cleared
two and confirmed a constructor slowdown. A cold legacy-constructor helper
restored the owned-input constructor diagnostic within the 5% gate and passed
an encrypted Type1 loading diagnostic. The complete retained implementation
completed all 46 original controls with 21 alternating pairs per case:
26 passed, 20 were inconclusive, and none was classified as a regression.
The TrueType constructor control passed at +0.10%, with an upper 95% bound of
+4.90%. The full performance gate remains unaccepted because uncertainty is
not a pass. No PR or issue closure is claimed.

## 03-packs.md

# feat(font): load bounded fallback font packs lazily

## Problem or use case

System fonts are unavailable in browser WASM and are often absent in containers. Callers need a portable way to supply fallback fonts without decoding a full font collection on every page.

## Proposed behavior

Add font packs containing licensed font bytes or bounded compressed font bytes with coverage metadata. Decode a requested pack once, share its bytes, and admit it within configured memory limits. Provide native and WASM memory registration examples, plus optional host-side download/cache integration using explicit caller-selected sources. The synchronous rendering path performs no network requests. Raw TTF/OTF does not require decompression; compressed containers must be identified explicitly.

## Acceptance criteria

- Lazy admission, reuse and decoded-size limits have deterministic tests and benchmark cases.
- Failed and over-budget decoding is bounded and does not poison subsequent valid loads.
- A real redistributable fallback font can render an otherwise missing CJK character in a headless configuration.
- Acquisition is opt-in, has a persistent cache/version identity where applicable, and supports offline reuse.
- Registered/downloaded fonts refresh coverage misses without restarting the process.
- Cold acquisition/decode cost is reported separately from warm rendering.

Depends on the provider sub-issue.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

Implemented locally:

- Lazy raw/zlib packs with shared concurrent decoding and cached failures.
- Upfront decoded-capacity reservations plus a separate encoded-byte budget;
  failed face admission cannot bypass retained-memory accounting.
- Explicit pack aliases, shared TTC programs, optional coverage hints and
  generation-based document refresh after registration.
- Native raw/pack APIs and WASM registration; actual wasm32 target check passes.
- Opt-in HTTPS host tools with digest pinning, bounded acquisition, persistent
  compressed caching and verified offline reuse. Rendering performs no I/O.
- A licensed Noto Sans SC 2.004 subset with source URL, SHA-256 and OFL notice.
  Headless tests check real CJK outlines, extraction, PDF advances/positions,
  CPU ink and all twenty SVG contours, including registration after a miss.

Eight pack tests, two real-CJK integration tests, six Python acquisition tests
and four browser-helper tests pass. Affected-target clippy passes. All fourteen
dedicated release benchmark cases have been measured. Tiny-fixture medians:
cold zlib decode 17.285 us, cached decode 0.006 us, cold selection 23.721 us,
warm selection 0.434 us, cold page setup 70.892 us, warm page setup 6.894 us.
These are initial workload baselines, not regression-acceptance evidence.

One separately measured live acquisition took 0.618 s; verified offline reuse
took 0.344 ms. The raw fixture is 2,568 bytes and its cached pack is 1,687 bytes.
This does not establish full-font acquisition performance; the full upstream
font transfer timed out on this host. All original 46-case regression controls
completed for the pack-enabled build with 21 alternating pairs each: 37 pass,
9 inconclusive, no classified regressions. Performance acceptance remains open.

Final integration exposed additional regressions in the real-CJK setup cases:
v4 cold +14.45%, late +15.57%, warm +89.56%. Rendering itself passed. Duplicate
outline checks and repeated descriptor resolution were investigated rather
than hiding these cases. The current candidate shares bounded original-glyph
checks through weak font identities, parses one face per text operation and
reuses the FontDescriptor. Focused cold/late checks pass at -1.30%/-1.61%; warm
is -2.09% but its upper interval is +5.47%, so it remains unaccepted. Complete
14-case pack and other current-source suites are running; prior results are
retained separately. Final acceptance is still pending.

## 04-fallback.md

# fix(font): resolve missing glyphs by coverage while preserving PDF placement

## Problem or use case

Selecting one substitute face does not recover individual missing characters. Some failed mappings reuse a raw character code as a GID, which can draw an unrelated glyph. A GlyphRun currently references one font, so fallback must explicitly select the actual drawing face.

## Proposed behavior

Resolve usable original glyphs first, then select a compatible fallback face using reliable Unicode/collection mappings and coverage. Split runs as needed while preserving original advances, vertical origins, transforms, spacing, clipping and extraction. Cache successful and unsuccessful resolution with provider-generation invalidation and bounded memory.

## Acceptance criteria

- Mixed Latin/CJK/supplementary characters and a missing symbol can select different faces within one original PDF text operation.
- A missing mapping cannot silently become an unrelated substitute GID.
- Empty valid glyphs such as spaces are not treated as broken outlines.
- Multi-scalar mappings and variation selectors have explicit, tested semantics; original positioned PDF text is not globally reshaped.
- Forms, text clipping, CPU/GPU/SVG and text extraction agree on actual glyph selection.
- Writer APIs can use the same coverage selection when creating multilingual text and embed each selected subset.
- Benchmarks cover cold misses, warm hits, repeated misses, mixed runs and unchanged embedded text.

Depends on the font-pack sub-issue.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Working-tree progress

Initial foundations are implemented and functionally validated:

- Unmapped simple-font character codes no longer become unrelated raw GIDs;
  Type3 retains its valid character-code addressing.
- Missing/empty substitute CID mappings remain explicitly empty, preventing
  accidental CID-to-substitute-GID identity.
- Multi-scalar ToUnicode strings preserve full extraction without drawing
  only the first scalar. Valid empty spaces retain their mapping and advance.
- Missing Type0 programs retain composite segmentation, CMaps, horizontal
  widths, explicit zero widths, vertical origins and vertical advances.

Five new regressions, full affected font/document/content/benchmark suites,
clippy and 68 existing font/provider/pack smoke cases pass. The corresponding
46-case reference performance comparison completed with 45 pass and one
inconclusive TrueType-construction case (+1.76%, upper 95% bound +11.18%).

The provider now also exposes explicit character/variation coverage selection,
registered bold/italic matching, lazy-pack coverage hints and a bounded
4,096-entry positive/negative cache invalidated by registration. Six coverage
integration tests and one cache-bound test pass, along with font tests, clippy
and the actual WASM target check. A deterministic 776-byte cmap format-14
fixture covers default and non-default variation sequences. All eight new
coverage benchmark cases have been measured; their original controls completed
with 28 pass and 18 inconclusive, without a classified regression.

Automatic page resolution, adjacent face splitting and union text clipping are
implemented. Five page tests and five resolution tests passed, covering
mixed Latin/CJK/supplementary/emoji, spaces, unrecoverable misses, original
placement, transforms/TJ/spacing, vertical Forms, cache limits, damaged
outlines and late registration. CPU clip-only output is pixel-identical to
ordinary text drawing; SVG retains every visible fixture contour. The actual
WASM target builds. GPU transforms now carry positioned glyph.y, and real
Apple M5/Metal tests passed horizontal and vertical per-glyph placement/ink.

All nine initial page-fallback workloads completed. Selected medians on a
256-code mixed fixture: cold setup 401.668 us, warm document setup 302.797 us,
warm page-cache interpretation 266.680 us, CPU rendering 3.808 ms. Eight-row
vertical setup measured 104.226 us and sixteen-row clipping 512.917 us. These
are new-workload baselines, not comparative performance acceptance.
All original 46 controls passed for the archived page implementation, with
21 alternating pairs per case and a largest upper 95% bound of +1.53%.

Newer source adds scalar multilingual writer authoring, selected TTC-face
extraction, explicit-encoding miss corrections and empty-clip semantics. Writer
round trips and all writer tests pass. Three writer benchmarks completed:
64 mixed scalars 1.953 ms, 64 single-face scalars 0.988 ms, cached unsupported
text 1.968 us. Unsupported scalars/selectors do not partially modify the PDF.
The API explicitly does not perform shaping or bidi reordering.

The latest integration passed 580 affected font/document/content/CPU/SVG tests,
eight page tests including real GPU checks, two writer tests, 55 GPU unit
tests and the actual WASM target check. Patterned text matches one explicit
union clip; a space-only clip blocks painting and restores correctly on Q
across CPU/GPU/SVG. A legacy test incorrectly treated WinAnsi yen as a
MacRoman bullet; it now verifies the correct distinct glyphs. Latest lint and
all 88 font smoke cases pass. Current-source performance controls are running
for both the eight existing provider workloads and all 46 original controls;
the earlier page pass does not validate the later writer/encoding/clip changes.
The provider searches registered faces/packs without per-glyph system scans
or network I/O. This issue remains open pending full integration and acceptance.

An earlier eight-provider control run found three repeatable regressions:
late registration +12.59% (95% +11.72..+13.08%), registered cold page loading
+10.69% (+9.92..+11.35%), and registered warm loading +13.96%
(+13.28..+14.52%). The other five cases passed. Follow-up optimization removed
per-glyph Arc churn/redundant font lookup, reused adjacent equal-code decisions,
and kept optional painting/clipping off the ordinary decoding stack. All eight
provider cases then passed the strict paired gate: empty provider +1.69%
(upper +2.46%), late registration +3.63% (upper +4.20%), cold loading +2.73%
(upper +3.52%), warm loading +4.31% (upper +4.75%). The matching original suite
had 41 pass and five inconclusive cases, with no classified regression. These
archived results precede CMap integration; current-source controls are being
repeated. Full campaign acceptance remains open.

The final provider candidate now passes all eight controls with 21 AB/BA
pairs: empty provider -0.20%, late registration +2.17%, cold loading -0.86%,
warm loading +2.75%, registration -3.07%, duplicate registration -1.43%,
name hit -4.32%, name miss +1.39%. The largest upper 95% bound is +3.76%.
All prior observations remain preserved; current original/CMap controls still
have unresolved uncertainty. The broad final command reports 828 test passes
across 43 suites, with private-corpus early returns explicitly excluded from
coverage claims. Final clippy and wasm32 compilation pass.

## 05-cmaps.md

# fix(font): complete supported CJK CMaps and encoding coverage

## Problem or use case

The current predefined legacy CMap builds only an ASCII CID range. Double-byte CJK codes rely on Unicode decoding for substituted fonts, leaving the embedded-font code-to-CID path incomplete. GBK2K is classified as two-byte GBK and HKscs-B5 uses CP950, which does not establish full coverage.

## Proposed behavior

Use authoritative redistributable Adobe mapping resources or verified compact generated equivalents for supported code-to-CID and CID-to-Unicode mappings. Handle supported multibyte and supplementary-plane forms without truncation. Explicitly reject or diagnose unsupported variants instead of routing them through an unrelated encoding. Keep generation reproducible and preserve resource provenance.

## Acceptance criteria

- Embedded and substituted legacy CJK PDFs resolve the same intended characters with correct CID-indexed widths.
- Four-byte GBK2K/GB18030-related cases, HKSCS and Japanese/Korean variants have reference-backed coverage.
- Unicode/CID/GID namespaces remain distinct; Identity-H is never assumed to mean Unicode.
- Generator checks and tests cover boundaries, malformed sequences, usecmap and vertical variants.
- Benchmark both mapping throughput and full document rendering before replacing existing tables; compactness alone is not an optimization.

Depends on the glyph-fallback sub-issue.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.

## Implemented in the working tree

Pinned 91 byte-verified Adobe sources and both license notices in a 1,473,153
byte offline archive. The manifest retains revision, path, Git blob, size and
SHA-256 for every file. Encoding CMaps use cmap-resources revision
f5cf3bca7fdfeaceb77aa82847e974f2306c20b4; canonical collection Unicode resources
use mapping-resources-pdf revision 2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5.

The offline generator resolves inheritance/overlays and emits 84 named CMaps
and five canonical collections. Runtime data is 2,398,896 mapping bytes plus
18,550 text bytes. Sparse/dense pages, Unicode-source CID-only pages and shared
sections avoid allocating one large map per font. Seven generator tests pass;
regeneration is byte-for-byte reproducible. Test-only source digests verify all
1,190,674 encoding entries and 113,496 canonical strings independently of the
packed page layout.

The reader now separates source code, CID, Unicode and GID. W/W2 use the PDF
CID for embedded and substitute faces. Known Adobe collections recover text
without ToUnicode; full variation sequences remain intact. Embedded usecmap
overrides preserve lengths and declaration order. Exact-name lookup replaces
broad-prefix guesses; GBK2K four-byte and HKSCS anchors are source-backed.

The writer emits predefined source codes and source-keyed ToUnicode while
indexing W/W2/CIDToGIDMap by collection CID. CID-keyed CFF retains its charset
and Registry/Ordering/Supplement and never receives a CIDToGIDMap key.
Incompatible predefined CFF collections/charsets fail explicitly. New tests
also exposed and fixed zero vertical advances and an extra 90-degree text
rotation when authoring vertical text. TTC extraction validates aggregate
table bytes before copying overlapping records.

526 affected unit tests and ten CMap/writer integration tests pass, including
portable embedded/substitute no-ToUnicode pages, nonuniform widths and a valid
nonidentity CID-keyed CFF. Clippy and the actual wasm32 target build pass.

All 31 new font_cmaps benchmark cases completed on Apple M5/macOS 26.6/Rust
1.98.1. Lookup batches contain 256 operations: 18.956–31.346 us for the six
named-map cases and 2.568 us for Identity. Parse-through-CPU-render pipelines
contain 24 glyphs: 387.707–446.304 us except Identity collection substitution
at 2.071 ms, which currently builds canonical CID coverage at font load.
These establish corrected-semantics baselines, not speedup claims.

The uniformly sparse directory was rejected after six original legacy-query
controls showed confirmed 19.61–82.39% regressions. All 19 completed cases are
preserved as explicitly rejected partial evidence. A replacement uses direct
one/two-byte directories and avoids string-valued results for scalar/CID
queries. All six targeted old-encoding diagnostics passed, roughly 66–73%
faster than the original controls. The source oracle remains unchanged.

The replacement has 2,413,284 mapping bytes plus the same 18,550-byte text
pool. The benchmark executable grew from 6,796,368 bytes before CMap work to
9,294,576 bytes. Source compactness does not remove that linked-size cost.
Canonical CID substitution now resolves on demand instead of scanning the
entire collection at font load, improving its 24-glyph pipeline by 81.48%.

All 31 current CMap controls completed: 23 pass, eight inconclusive, no
classified regression. Final provider controls pass all eight cases and final
GPU controls pass all six cases. Complete original controls are still running;
uncertain cases receive one fixed extension with all prior observations pooled.
Full acceptance remains open. Historical private PDFs remain absent (0/19).

## 06-cache.md

# perf(font): optimize measured glyph work and validate the complete fallback pipeline

## Problem or use case

The GPU path extracts an outline before checking the atlas, and the CPU path repeats outline work per glyph instance. Prior experiments show that indiscriminate bitmap or Face caching can have marginal or negative returns. The complete fallback pipeline therefore needs measured optimization rather than a blanket cache.

## Proposed behavior

Measure cache hit opportunities, query the GPU atlas before outline extraction where valid, and retain bounded outline/resolution caches only when the measured workloads benefit. Key caches by actual font identity, face index, variations, glyph and applicable device parameters. Pin font resources referenced by live display lists.

## Acceptance criteria

- Existing and new font workloads are measured on the same host against pre-change controls.
- Rotation, shear, mirroring, vertical positioning, variation instances and multiple documents cannot alias cache entries.
- Missing-glyph recovery is verified visually and semantically, not only by timing.
- Native, WASM, CPU, GPU, SVG and authoring integrations have appropriate build/test coverage.
- Publish an English implementation/benchmark report mapping each child issue to actual evidence, including any unavailable historical corpus or hardware.
- Repeatable regressions above the agreed 5% gate are fixed before completion.

Depends on the CMap sub-issue.

## Implemented and measured so far

The GPU renderer now checks the page-local atlas before extracting a glyph
outline. Cache keys retain the drawing FontId, glyph ID and independent x/y
device sizes. Rotation, shear and mirroring keep the vector path. Atlas state
is scoped to the page, so it cannot leak IDs across document font caches.
55 GPU unit tests and eight mixed-face page tests pass, including real Apple
M5/Metal placement and clipping. MSAA is x4; timestamp queries are unavailable.

The six-case GPU baseline and candidate archives record actual adapter
identity and verify it in every paired process. The final 21-round AB/BA
comparison passed all six cases: unique CJK -8.73%, repeated Latin -17.19%,
mirrored -0.90%, mixed supplementary -12.69%, sheared -0.64%, vertical mixed
-6.02%. The largest upper 95% bound is +2.79%. These are render/readback wall
times on the portable fixtures, not isolated GPU timestamp measurements.

The first CMap baseline exposed 2.071 ms for Identity collection substitution,
versus approximately 0.4 ms for other 24-glyph pipelines. Canonical CIDs now
resolve on demand using an already parsed face; explicit ToUnicode misses
still suppress canonical recovery. The final paired measurement improves that
pipeline by 81.48% (95% -83.23..-80.58%). All 31 CMap controls completed: 23 pass,
eight inconclusive, no classified regression. The complete original controls
are running. One fixed 42-pair extension is planned for uncertain cases, with
all earlier observations retained by the tested pooling tool. Full campaign
acceptance remains open.

## Further regressions and current candidate

Completing all dedicated suites exposed v4 regressions beyond the original
controls: real CJK pack cold/late setup +14–16%, warm setup +89.56%, cold coverage
hit/miss +18.36%/+8.82%, late registration +6.84% and vertical fallback +5.45%.
Those results remain preserved and v4 is not accepted, even though its original
46 controls, provider 8, GPU 6 and writer 3 cases passed.

The v5 candidate separates mapping from outline validation, caches original
glyph usability through bounded Weak LoadedFont identities, shares a parser
per text operation, and avoids full Face parsing for coverage misses. Checks
retain no font program bytes and are bounded by 4,096 glyphs/1,024 identities.
It also removes repeated composite FontDescriptor/CIDSystemInfo resolution and
sizes page-font reservations to actual resources. Descriptor axes and damaged
outline fallback retain regression coverage.

Focused controls now pass cold coverage hit (-9.22%), cold miss (-32.51%), late
registration (+0.90%), real CJK cold (-1.30%) and late (-1.61%). Real CJK warm
setup remains inconclusive (-2.09%, upper +5.47%). Four-byte GBK prefix lookup
also received a 1,028-byte high-byte directory; all source mappings still match.

840 reported test passes, affected clippy and the actual wasm32 build pass.
Historical private inputs remain absent (0/19). All nine current-source
benchmark suites (136 cases) are being compared before claiming acceptance.
The separate allocation/copy diagnostic remains unresolved: identical-binary
calibration itself has a wide interval. It is not silently treated as a pass.

The complete v5 coverage suite passes all eight cases. Its real-CJK pack setup
cases also pass (cold -1.78%, late -1.33%, warm +2.90%, upper +4.13%). But the
shared original checks were ahead of existing per-page decisions, regressing
mixed-page setup and warm interpretation by 6–17%. V5 was stopped and its
partial CMap observations preserved. The current correction restores page
decision priority and skips operation-wide checks after a mixed-run cache
exists. Complete fallback and pack comparisons are running. No acceptance is
claimed by combining passes from different candidate binaries.

The cache-order candidate passed all nine fallback controls, but a fixed pack
confirmation identified lazy-registration overhead (+8.64%). Subsequent cache
boxing alternatives were measured and rejected when they left registration
uncertain or regressed warm variation lookup. The current design keeps the
coverage hash map directly accessible and lazily allocates original checks
inside the existing registry lock. It passes all eight coverage cases; focused
real-CJK cold/late/warm and lazy registration also pass. Complete remaining
suites are running for this exact source. The latest full correctness command
reports 844 passes across 46 suites; clippy, WASM, generators, host acquisition
and browser helper checks pass. Performance acceptance remains open.

## Validation and performance gate

- Use deterministic, redistributable fixtures checked into the repository; missing inputs must fail rather than silently skip.
- Record release-profile before/after measurements on the same host, with the same fixture hashes, toolchain, settings and case inventory.
- Treat a repeatable regression above 5% in an existing workload as a release blocker. Investigate noise with independent repeated runs; do not hide regressions by changing fixtures or dropping cases.
- Verify glyph identity, text extraction, original PDF advances/positions and CPU/SVG/GPU behavior where affected. Passing timings with missing text is not a performance improvement.
- Extend the dedicated font benchmark before accepting new behavior; run the existing pipeline benchmarks as regression controls.
