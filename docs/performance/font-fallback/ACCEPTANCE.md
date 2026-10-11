# Acceptance audit

Objective: implement the six linked font work packages, add missing benchmarks,
retain performance without a repeatable regression above the agreed 5% gate,
and keep comments, issue text and implementation reports in English.

This audit is **complete for the user-selected Linux acceptance platform**.
The current implementation passes the complete 57-case Linux font inventory
and the separately repaired 135-case Linux pipeline inventory. The historical
macOS heap-copy regression remains retained as a platform-specific diagnostic
limitation; it is not relabeled or used to claim macOS acceptance. The later
benchmark-only repairs change no production library code, and exact executable
bindings preserve every earlier font outcome.

| Requirement | Authoritative evidence | State |
| --- | --- | --- |
| English parent issue and six native sub-issues | GitHub issue #46 and its sub_issues API returned #47–#52, all open, with the requested English titles | Verified |
| Portable benchmarks with fixed meanings and complete inventories | fonts 46; packs 14; coverage 8; fallback 9; CMaps 31; provider 8; GPU 6; writer 3; diagnostics 11. Snapshot input hashes, expected_cases and executable hashes are recorded | Complete inventory measured and exported |
| Shared validated memory fonts | provider/pack tests cover aliases, TTC face indices, malformed fonts, shared ownership, byte/name/face budgets and late registration | Correctness verified |
| Bounded lazy decoding, explicit acquisition and offline reuse | pack tests; test_fetch_font.py; browser font-packs.test.mjs; pinned licensed Noto subset; native/WASM entry points | Correctness verified; whole-font network cost not claimed |
| Reliable missing-glyph recovery and original PDF placement | font_fallback/font_packs/font_cmaps tests verify face/GID, W/W2, TJ/spacing, transforms, rises, Forms, selectors, missing glyphs and empty spaces | Correctness verified |
| CPU/GPU/SVG clipping and placement | actual Apple M5/Metal checks; union/empty/pattern clip tests; CPU ink/position and SVG contour assertions | Verified within documented AA limitations |
| Writer selection, embedding and font namespaces | multilingual TTF/TTC round trips; nonidentity CID-keyed CFF; predefined collection CIDs; 2000-UPEM vmtx/VORG origins; unsupported-text atomicity | Correctness verified |
| Exact supported CJK mappings | pinned Adobe manifest/licenses and deterministic source ZIP; exhaustive source digests for 1,190,674 encoding entries and 113,496 canonical strings; eight generator tests | Verified |
| Bounded cache identity and ownership | resolution unit tests verify weak ownership, changed maps/variations, negative checks, 4,096 glyph and 1,024 font bounds; generation and page-ID tests | Verified |
| Valid emitted sfnt/TTC and allocation bounds | checksum tests, selected-face extraction, duplicate/overlapping table rejection before copying | Verified |
| Native, WASM, lint and host-helper validation | font-writer-preflight-full-tests.log reports 847 passes across 46 suites; font-writer-preflight-full-clippy.log, font-writer-preflight-full-wasm.log; unchanged helpers in font-acceptance-tool-tests.log | Passed; private-corpus early returns excluded from coverage claims |
| Root helper source identity after benchmark repairs | evidence/root-helper-bindings.json compares all five root Python helpers with the frozen transfer inventory and rechecks the prior test-log digest | Exact bytes match; complements the Cargo.toml/crates-only source fingerprint without repeating tests during timing |
| Current-source performance on every suite | All current captures, fresh changed-binary reports, retained-data confirmations and exact bindings are exported in evidence/writer-preflight-precision-pairs.json | All 125 functional Mac controls pass; the separate Mac heap-copy diagnostic remains a documented limitation rather than Linux acceptance evidence |
| Existing synthetic pipeline correctness | zpdf-benches perf_gate and benchmark_budget pass in both isolated Linux trees; all 135 stages cases execute in test mode | Passed; these tests alone do not establish relative performance |
| Existing synthetic pipeline relative timing | PIPELINE_RECHECK.md, PIPELINE_MEASUREMENTS.md and evidence/pipeline-pairs.json retain all 21 full-suite pairs; independent audit verifies 57,120 raw samples | 131 pass, four inconclusive, zero regression; every end-to-end case passes, but the complete matrix is not accepted |
| Repaired bounded-output pipeline protocol | PIPELINE_BOUNDED_MEASUREMENTS.md, pipeline-bounded-pairs.json, pipeline-bounded-audit.json and exact benchmark-bounded-bindings.json | All 135 fresh cases pass; largest upper bound +4.11%; 58,380 raw samples independently verified; never pooled with SmallInput |
| Mac allocation-callsite precision diagnostic | copy-callsite-precision-results.md and evidence/copy-callsite-precision.json retain 512 pairs per condition and all 40,960 samples | All four probe calibrations pass; both shared and distinct modes pass; diagnostic does not clear the original old/new copy_heap regression |
| Honest historical-corpus scope | 0/19 private PDFs available; portable and historical results kept distinct in reports | Limitation explicitly retained |
| Reproducible evidence, including rejected work | PRECISION_MEASUREMENTS.md and evidence/writer-preflight-precision-pairs.json export every current pair/binding; preceding evidence remains in history and evidence/adjacent-repeat-pairs.json; raw files/executables retained locally | Current and preceding inventories exported; rejected and platform-specific results remain visible |
| Final English report and synchronized issue status | REPORT.md, STATUS.md, measurement tables and machine-readable evidence reflect current results; native sub-issue relationships verified | User authorized the GitHub issue synchronization; bodies are published in English without local paths and retain the measured performance data |

## Performance decision rules

The same archived binaries run in alternating AB/BA order. Reports validate
host, input, inventory, binary and GPU identity; default/background protocols
cannot be pooled. The default-scheduler acceptance run uses 21 pairs, ten
Criterion samples, 0.1 s warmup and at least 0.25 s measurement per observation.
The upper paired bootstrap 95% interval must remain at or below +5% to pass.

An inconclusive case is not a pass. Fixed extensions retain every earlier
observation. A diagnostic subset cannot establish a full-suite pass, and a
confirmed regression requires investigation. Identical-binary calibration
checks both directions; its failure cannot be presented as evidence of a
production improvement. No threshold, fixture or case is removed to obtain
acceptance.

The old copy/allocator diagnostic had a classified regression while an
identical-binary calibration also had a wide interval. That evidence remains
visible and is not silently cleared. The complete macOS diagnostic suite and
its fixed confirmations are retained as recorded. Longer-window and background
same-binary calibrations initially remained inconclusive. Fixed 512-pair
same-path and different-path calibrations now pass, but the 575-pair retained
before/after heap-copy result remains +16.86% (95% +10.40..+26.20%). The other
ten diagnostic cases pass. An independent probe
reproduces wide variation in preallocated copying while allocation/free is
comparatively stable. All 125 functional controls pass. The equivalent Linux
diagnostic passes under the user-selected acceptance platform, and the macOS
result is retained unchanged rather than relabeled or cleared by a relaxed
threshold.

Installed allocator inspection and a fixed single-allocation intervention now
identify a macOS type-bucketing contribution to this pure-copy workload.
Changing only the source bucket increases time in both binaries; all samples,
failed attempts and the separate protocol remain in the investigation report.
This does not establish a passing 5% before/after bound or clear the original
575-pair result, which is retained as a platform-specific limitation. The
Linux acceptance scope was selected explicitly by the user; allocator
interventions are not a shipping workaround and no gate was relaxed.

The subsequently authorized independent Linux recheck is complete and passes
all 57 common cases against production baseline 137ab2b with matched workloads
and pinned third-party packages. Both original font controls (46/46) and
diagnostics (11/11) pass, including copy_heap and the 1,109-pair copy_aligned
confirmation. Native release validation reports 788 passes across 42 suites.
All source/binary hashes and 60,200 raw samples are verified and archived.
See [the complete Linux table](LINUX_MEASUREMENTS.md). This establishes Linux
acceptance for that inventory. It does not run the missing hardware GPU cases,
claim the absent historical corpus, or relabel the recorded macOS regression.

The separately repaired Linux stages protocol subsequently passes all 135
cases, with all 58,380 samples audited. A further isolated Mac probe tests
shared versus distinct allocation callsites without changing the original
diagnostic. All four same-binary calibrations remain inconclusive, so that
candidate is not adopted. Its full 1,680-sample evidence is retained in
copy-callsite-results.md and evidence/copy-callsite.json.
The unchanged scope was then pursued with a fixed 512-block
precision extension of that probe, retaining all 21 initial blocks. It is now
complete, and the independent audit verifies all 40,960 raw samples. All four
same-binary calibrations pass; both shared and distinct modes pass. These probe
builds are not the old/new font implementations and do not clear the original
regression. See copy-callsite-precision-results.md.

The user selected the Linux environment as the final performance acceptance
platform. Under that scope the 57-case Linux font inventory and the repaired
135-case Linux pipeline inventory both pass, with every raw sample audited and
independently recomputed. The macOS heap-copy diagnostic is retained as a
documented platform-specific limitation and does not block the Linux
acceptance decision. No gate, fixture or case is removed, no measurement is
relabeled, and no binary is perturbed only for allocator placement.

## Exact executable reuse

A fresh source capture may produce exactly the same archived executable as an
earlier candidate. Existing timings then remain measurements of those bytes,
provided fixture/workload hashes, complete case inventory, host/toolchain and
measurement protocol also match. Both archives must retain their executable
hashes and distinct source fingerprints. The binding must be explicit; an
unchanged benchmark source file alone is insufficient.

The writer preflight captures demonstrate this for packs, coverage, page
fallback and diagnostics in `font-writer-preflight-binary-bindings.json`.
Retaining evidence includes every unsuccessful verdict and observation, not
only passes. No earlier report or measurement timestamp is rewritten. Current
exports must show both the measured capture and its current-source binding.
Changed executable bytes require new measurements and cannot use this rule.

## Publication and workspace state

Branch: codex/font-fallback-performance, based on 137ab2b. The campaign is
committed on this branch and published as a pull request against `main`; the
tracking issue #46 carries the link. All six sub-issues remain open and no
issue is closed by the pull request. The user authorized the GitHub issue
synchronization; that content is English, carries the measured performance
data and contains no local paths. The original JBIG2 branch and the benchmark
reference worktree remain preserved. Before managed worktree cleanup, retain
every required ignored executable and raw result directory.

## Published evidence redaction

The issue bodies and the machine-readable exports under `evidence/` are
published without host-specific identifiers. Absolute workspace prefixes are
replaced with the `<workspace>` placeholder and one recorded host name with
`<host>`. Sample values, recorded hashes, verdicts, inventories and the byte
counts of the individual observations are unchanged; only host-local path and
host-name strings were rewritten. Unredacted originals remain outside the
repository alongside the raw archives.
