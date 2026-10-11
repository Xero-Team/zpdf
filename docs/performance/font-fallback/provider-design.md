# Shared font provider design

Work package: [#48](https://github.com/Xero-Team/zpdf/issues/48).
Status: implemented in the working tree. Current measurements and outstanding
acceptance items are recorded in STATUS.md; this document describes the design.

## Ownership and API boundaries

The font provider owns immutable `Arc<[u8]>` font programs and validated face
metadata. Registration is explicit and fallible. It accepts a face index for
TTF/OTF collections and returns an identity stable for the provider's lifetime.
Registration validates size, face index, font structure and registry capacity
before admission. Memory-only mode never scans a filesystem. System-font
discovery remains an optional source using the existing matching behavior.

`PdfDocument` receives an optional shared provider through an additive setter
and builder method. Existing constructors keep the provider absent, preserving
the current embedded/system-font path. Page `FontCache` objects carry the same
provider so nested Form XObjects and inline appearance font dictionaries do not
silently revert to system-only lookup. Direct font-loader callers get explicit
provider-aware variants; current functions remain compatibility wrappers.

`LoadedFont` continues to own the selected program bytes. Registering a font
does not require keeping the caller's original buffer alive, and destroying a
provider cannot invalidate fonts retained by a page/display list. Live font
IDs must not be evicted merely to reclaim cache capacity.

## Selection and updates

Selection first considers exact PostScript/full names, then family/style,
aliases and registered fallback order. Coverage-aware character selection is
added in #50; whole-face matching in #48 must not claim to repair individual
missing glyphs. Embedded font programs retain priority.

Successful registration increments the provider generation. A document detects
the changed generation before subsequent page font loading, so earlier
substitute/placeholder results cannot hide a newly registered matching font.
Already-created page caches remain valid snapshots. The glyph-resolution cache
introduced in #50 also keys or invalidates negative results by this generation.

Registration and queries use synchronization only for explicit providers. The
legacy embedded-font fast path must not acquire a registry lock or compute a
font hash per glyph. Font byte identity and metadata are established at
registration/load time rather than in the rasterizer.

## Required evidence

| Requirement | Correctness evidence | Performance evidence |
| --- | --- | --- |
| Memory registration | Caller buffer lifetime, malformed bytes, invalid face index, size/capacity limits | Cold registration |
| Shared programs | Pointer identity and TTC face selection | Warm face selection and multi-page reuse |
| Application control | Memory-only mode resolves a supplied font with no system lookup | Registered-name hit and miss |
| Resource propagation | Page, nested Form XObject and inline appearance dictionaries use the same provider | Repeated form font loading |
| Late registration | A prior miss is replaced in a newly loaded page cache; old cache remains usable | Generation-change cost separately from warm hits |
| Compatibility | Existing font, content, writer and renderer tests | All existing 46 font controls plus portable pipeline controls |

New provider workloads are added before accepting provider behavior. Existing
workload names and fixture meanings remain intact. The public facade exposes
the provider types; WASM memory registration is covered by the integration
work in #49/#52.
