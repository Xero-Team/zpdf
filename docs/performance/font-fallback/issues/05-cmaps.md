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
SHA-256 for every file. Encoding sources use cmap-resources revision
f5cf3bca7fdfeaceb77aa82847e974f2306c20b4; canonical Unicode resources use
mapping-resources-pdf revision 2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5.

The offline generator resolves inheritance/overlays and emits 84 named CMaps
and five canonical collections. Current schema 5 uses direct one/two-byte
page directories, sparse longer directories and a high-byte bucket for legacy
four-byte lookup. Runtime data is 2,414,312 mapping bytes and an 18,550-byte
UTF-8 text pool, shared statically without per-font copies. Eight generator
tests and byte-identical regeneration pass. Independent source digests verify
all 1,190,674 encoding entries and 113,496 canonical strings.

The reader distinguishes source code, CID, Unicode and GID. W/W2 use the PDF
CID for embedded and substitute faces. Known Adobe collections recover text
without ToUnicode; explicit ToUnicode takes precedence and full canonical
sequences remain available. Source lengths and usecmap declaration order are
retained. Exact-name lookup replaces broad-prefix guesses. GBK2K four-byte,
HKSCS and supplementary UTF-16 anchors are source-backed.

The writer uses collection CIDs for W/W2 and TrueType CIDToGIDMap, with
source-keyed ToUnicode. CID-keyed CFF preserves its charset and ROS and never
receives an invalid CIDToGIDMap. Incompatible collections/charsets fail.
Vertical authoring retains upright glyphs, PDF defaults and normalized
vmtx/VORG origins. TTC/sfnt rebuilding bounds aggregate table copies and
recomputes checksums.

The original sparse runtime directory was rejected for measured legacy-query
regressions. Its observations remain archived. The current implementation
also resolves canonical CIDs on demand, avoiding a full collection scan at
font load.

## Archived adjacent-repeat measurements

All 31 CMap cases completed with 21 AB/BA pairs and default scheduling:
30 pass, one inconclusive, no classified regression.
Report: font-repeat-final-cmaps-paired.json.

- GBK2K four-byte decode -17.17%; lookup -38.01%.
- HKSCS decode -26.99%; lookup -48.96%.
- UTF-16 decode -0.26%; lookup -10.67%.
- Identity collection substitute pipeline -74.03%.
- All three writer controls pass: nonidentity CFF +1.54%, predefined horizontal
  +0.33%, predefined vertical +1.00%.
- Vertical JIS embedded rendering is inconclusive: -0.13%, with a 95% interval
  of -8.25..+7.83%. A fixed confirmation will retain every original observation.

This first-run uncertainty required a fixed confirmation. The exact
candidate's full correctness command reports 846 passes across 46 suites;
warnings-denied clippy and actual wasm32 compilation pass. Historical private
PDFs remain absent (0/19). Full campaign acceptance remains tracked in #52.

The fixed 42-pair confirmation is complete. Retaining all first-run data gives
63 pairs for vertical JIS embedded rendering: -0.13%, 95% -2.82..+2.00%, pass.
All 31 CMap cases pass for that exact archived source.
Pooled report: font-repeat-final-cmaps-pooled.json. Broader campaign
acceptance remains open for other suites' unresolved cases.

The newer writer preflight candidate changes the linked CMap executable and
has now completed a fresh 31-case comparison: all pass. Report:
font-writer-preflight-cmaps-paired.json. Identity collection substitution
improves 75.20%; GBK2K lookup 37.14%; HKSCS lookup 49.84%. Embedded/substitute,
vertical and all three writer controls pass. The preceding pass is retained
separately rather than transferred to changed bytes. The user selected the
Linux environment as the final performance acceptance platform, where the
complete font and pipeline inventories pass; the retained macOS pure-copy
diagnostic is documented as a platform-specific limitation.
