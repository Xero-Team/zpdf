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

Implemented bounded raw/zlib packs with once-only concurrent decoding and
cached failures. Decoded-capacity reservations and a separate encoded-byte
budget include retained bytes after failed face admission. Shared TTC programs,
explicit aliases, optional coverage hints and provider generations support
late registration. Raw font bytes need no decompression.

Native and WASM registration APIs are available. Opt-in HTTPS helpers verify a
pinned digest, bound acquisition, retain a persistent cache and support verified
offline reuse. Rendering performs no networking. Pack, host-helper, browser and
real-CJK integration tests pass; actual wasm32 compilation passes.

The licensed Noto Sans SC fixture is a six-codepoint subset with provenance,
SHA-256 and OFL notice. Tests check outlines, text, PDF positions, CPU ink and
SVG contours, including late registration. Its raw size is 2,568 bytes and its
cached pack is 1,687 bytes. A prior live acquisition took 0.618 s and verified
offline reuse 0.344 ms. These measurements do not establish whole-CJK-font
download or decompression cost; the full upstream transfer timed out.

The exact current candidate passes all 14 pack controls with 21 AB/BA pairs
per case. Report: font-repeat-final-packs-paired.json. Real-CJK cold
setup is -2.61%, late setup -2.56%, warm setup +0.49% (upper +3.25%) and rendering
+0.11%. Lazy registration is -1.08% (upper +1.04%). All decode, cached failure,
limit and selection controls pass.

Earlier CJK setup and lazy-registration regressions remain archived. Bounded
original checks, page-decision priority and descriptor reuse address their
duplicate work. This exact-source suite passes; full campaign acceptance
remains pending in #52.

The writer preflight candidate's pack executable is byte-identical to this
accepted pack capture, with equal inputs/environment/inventory. The explicit
binding is retained in font-writer-preflight-binary-bindings.json.

The user selected the Linux environment as the final performance acceptance
platform, where the complete font and pipeline inventories pass the unchanged
5% gate. The retained macOS pure-copy diagnostic is documented as a
platform-specific limitation rather than a blocker for that decision.
