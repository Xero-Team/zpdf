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

Implemented validated Arc-backed TTF/OTF/TTC registration, cumulative alias and
program budgets, Unicode aliases, metadata-based style selection, stable face
IDs, provider generations and late-registration invalidation. Document pages,
nested/inline Forms and WASM retain the shared provider. Embedded programs keep
priority. Memory-only providers require no system font discovery.

Tests cover malformed fonts, invalid TTC indices, duplicate/concurrent
registration, atomic admission failures, limits and ownership. Composite
FontDescriptor axes survive embedded and substitute loading with inline or
indirect descriptors. Repeated simple codes retain every glyph and position
under spacing, rise, scaling and shear.

The current writer-preflight candidate passes all eight provider controls, with 21 AB/BA
pairs per case. Report: font-writer-preflight-provider-paired.json.
Changes against the original provider baseline:

- Empty-provider embedded page: -69.80%.
- Late registration: -64.62%.
- Registered cold page: -64.55%.
- Registered warm page: -72.44%.
- Registration cold: -5.48%; duplicate: -1.76%.
- Name hit: -2.81%; name miss: -1.49%.

The large page gains are on repeated-A workloads and come from avoiding
repeated mapping work; glyph and position assertions remain unchanged.
The upper paired 95% interval stays within the +5% gate for every case.
The fresh comparison measures the current linked executable. All 125 functional
font performance controls pass. The user selected the Linux environment as the
final performance acceptance platform, where the font and pipeline inventories
pass; the retained macOS pure-copy diagnostic is documented as a
platform-specific limitation.
