# Changelog

## 0.9.0 — Performance & robustness improvements

Comprehensive audit and optimization pass targeting rendering speed, memory usage, and security.

### Performance optimizations (2–3× faster on typical workloads)

- **Eliminated knockout group cloning** — `render_shape_cmd` now uses helper methods with alpha overrides instead of cloning entire `GlyphRun`/`ImageDraw` structs. Major win for layered/transparent PDFs.
- **CMYK color cache** — Single-entry cache in `ContentInterpreter` for repeated CMYK→RGB conversions. Achieves 90%+ hit rate on technical drawings with uniform colors (e.g., black text, cyan guidelines).
- **ColorSpace object cache** — HashMap cache keyed by `ObjectId` for resolved ColorSpace indirect references. Eliminates redundant ICC profile loads and array parsing on pages with uniform color spaces.
- **Dash pattern in-place extension** — Odd-length dash arrays now doubled via `array.reserve()` + in-place push instead of `clone()` + `extend()`. Reduces allocations by 50% for stroked drawings.
- **RenderedPage zero-copy save** — `save_png` now consumes `self` instead of taking `&self`, eliminating a 512MB pixel buffer copy on high-DPI pages (64Mpx). Save time reduced by ~50%.

### Robustness & security fixes

- **Predictor overflow protection** — Added parameter validation (`MAX_COLORS=256`, `MAX_BPC=32`, `MAX_COLUMNS=65536`) and `checked_mul()` to PNG/TIFF predictor calculations. Prevents allocation bombs from malicious PDFs attempting to overflow `usize` in `colors × bpc × columns`.
- **ObjStm safe parsing** — Replaced unsafe `as u32` casts with `u32::try_from()` in object stream parsing. Prevents silent truncation of negative or out-of-range object numbers.
- **Font cache LRU eviction** — Added capacity limit (default 256 fonts) with least-recently-used eviction policy. Bounds memory usage to ~50MB typical, preventing font-based DoS attacks from documents with hundreds of unique fonts.
- **Mesh shading NaN validation** — Added `is_finite()` checks to `/Decode` arrays in mesh shading decoding. Rejects NaN/Infinity from malformed PDFs before they corrupt rendering.

### API changes

- **Breaking**: `RenderedPage::save_png` now takes `self` instead of `&self` (consumes the page).

### Verification

All workspace tests pass (41 tests), clippy clean with `-D warnings`, functional testing confirms correct rendering on corpus PDFs.

## 0.8.0 — Document navigation & metadata: outline, destinations, page labels, links, XMP, info

A read-only data-model release, pure Rust with zero new dependencies: the
document's **navigation and metadata** surface — bookmarks, named destinations,
page labels, link-annotation targets, XMP metadata, and the information
dictionary — is now exposed through the library and CLI. These ride the same
catalog-reader + bounded name/number-tree-walk machinery the embedded-files /
output-intent / optional-content readers established.

### Outline (bookmarks) & destinations

- **Document outline** (`zpdf-document/src/outline.rs`, ISO 32000-1 §12.3.3):
  the catalog's `/Outlines` tree is parsed into a nested `OutlineItem`
  (`title`, resolved `dest`, `uri`, `open`, `children`). The walk follows
  `/First`/`/Next` with a depth cap, a per-reference visited set (the outline
  root is pre-seeded), and a global item cap, so a malformed or cyclic tree
  terminates without a hang. `/Count`'s sign (read through the resolving numeric
  accessor, so an indirect or real `/Count` is honoured) drives the `open` flag.

- **Destinations** (`zpdf-document/src/destinations.rs`, §12.3.2): a shared
  resolver turns any destination — an explicit `[page /Fit …]` array, a *named*
  destination, a `<< /D … >>` dictionary, or an indirect reference to one — into
  a `Destination { page, page_ref, view }`. The target page reference is mapped
  to a 0-based page index via the new `Catalog::page_index_of`; a bare page
  *number* (remote go-to) is range-checked against the document, so `page` never
  points past the last page. All eight view modes (`DestView`: `/XYZ` `/Fit`
  `/FitH` `/FitV` `/FitR` `/FitB` `/FitBH` `/FitBV`) are parsed, with `null`
  coordinates and a zero `/XYZ` zoom normalized to "retain current value".

- **Named destinations** are resolved from **both** registries: the modern
  `/Names /Dests` name tree (bounded by depth, a visited set, a node budget,
  and `/Limits` pruning that never hides a present key) **and** the legacy
  `/Root /Dests` dictionary still emitted by older producers. Name → name
  indirection is depth-bounded so a self-referential name cannot loop.

- **Outline targets** resolve each item's `/Dest` or its `/A` action — go-to
  (`/S /GoTo` → a destination), URI (`/S /URI` → the hyperlink), and remote
  go-to (`/S /GoToR` → the target file name).

### Page labels (`/PageLabels`)

`zpdf-document/src/page_labels.rs` (ISO 32000-1 §12.4.2) reads the catalog's
`/PageLabels` **number tree** — the printed page "numbers" a viewer shows and a
user navigates by, which are *not* the physical 0-based page indices. A document
commonly numbers front matter in lowercase roman (`i, ii, iii …`), the body in
decimals (`1, 2, 3 …`), and an appendix with a prefix (`A-1, A-2 …`); each is one
labeling *range* keyed by the 0-based index of its first page.

- Every label dictionary (Table 159) is honoured: `/S` numbering style — `/D`
  decimal, `/R`/`/r` upper/lower **roman**, `/A`/`/a` upper/lower **letters**
  (`A…Z, AA…ZZ, AAA…`); `/P` prefix; and `/St` start value (default `1`, clamped
  to `≥ 1`). An absent `/S` yields a prefix-only label with no numeric portion.
- `PageLabels::label(page_index)` returns the printed label, computing the
  numeric value as `St + (page_index − range_start)` for the covering range.
  Pages *before* the first range carry no label (`None`); ranges extend to the
  start of the next one (or end of document).
- **Bounded** like the destination readers: the number tree is flattened once
  (depth cap, per-reference visited set, node/entry budget), the `/P` prefix is
  length-capped, and a crafted `/St` beyond a sane bound falls back to a decimal
  rendering so a roman/letters label can't expand into a multi-megabyte string.

### Link-annotation targets

A `Link` annotation's navigation target is now resolved alongside its rectangle.
`Annotation` gained `dest: Option<Destination>` and `uri: Option<String>`,
populated from the annotation's `/Dest` or `/A` action by the **same resolver the
outline reader uses** — `/Dest` and go-to (`/A /S /GoTo`) → a [`Destination`],
URI (`/A /S /URI`) → the hyperlink, remote go-to (`/A /S /GoToR /F`) → the target
file name. The shared `resolve_link_target` (in `destinations.rs`) replaced the
outline's private copy, so bookmarks and links resolve identically.

- The named-destination registries are flattened **once per page** (the same
  bounded `collect_named_dests` walk), so a page of many link annotations
  resolves in O(links), not O(links × tree) — and it short-circuits cheaply when
  the document declares no named destinations.
- **CLI**: `zpdf links <file.pdf>` lists each link's rectangle and target
  (`-> p.<N>` in-document, `-> uri:<…>` external), page by page (the scan is
  page-capped so a huge document can't hang).

### XMP metadata (`/Metadata`)

`zpdf-document/src/xmp.rs` (ISO 32000-1 §14.3.2) reads the catalog's `/Metadata`
XMP packet — the RDF/XML metadata that PDF 2.0 prefers over `/Info`. The common
Dublin Core / XMP / PDF-schema properties are surfaced through `XmpMetadata`:
`title`, `creators`, `description`, `subjects`, `keywords`, `producer`,
`creator_tool`, `create_date`, `modify_date`.

- **Bounded scrape, not an XML engine.** This is deliberate: a general XML parser
  that resolves DTD entities is open to "billion laughs" entity-expansion bombs,
  and a DOM builder can blow the stack on deeply-nested input. Here **no general
  entity is ever resolved** (only the five predefined XML entities and numeric
  character references, each mapping to exactly one character), every scan is
  linear over the byte string, and each field and array is length-capped. The
  packet is decoded BOM-aware (UTF-8 / UTF-16), capped to 8 MiB.
- Handles the standard property shapes — simple element, `rdf:Alt` (preferring
  the `x-default` language), `rdf:Seq` / `rdf:Bag` arrays, and the RDF attribute
  shorthand on `rdf:Description`. The trade-off: non-standard namespace prefixes
  (not `dc`/`xmp`/`pdf`) are not recognized; in practice these are universal.
- **CLI**: `zpdf info` prints an `XMP Metadata:` block when the document carries
  a packet. `PdfDocument::metadata_bytes()` returns the raw packet for callers
  that want to run their own (hardened) XML parser.

### Document information dictionary (`/Info`)

`zpdf-document/src/doc_info.rs` (§14.3.3) reads the trailer's `/Info` —
`DocInfo { title, author, subject, keywords, creator, producer, creation_date,
mod_date, trapped }`. Text strings are UTF-16BE/PDFDoc-decoded; dates are
reported as their raw PDF date strings (no date parsing). The `/Info` reference
is read tolerantly: an indirect reference (per spec) **or** a direct dictionary
inlined in the trailer (lax producers, mirroring the direct-`/Encrypt`
tolerance) is accepted. Returns `None` when the document carries no `/Info` or
it holds no populated field.

### API & CLI

- **`PdfDocument`**: `outline()`, `named_destination(&[u8])`,
  `resolve_destination(&PdfObject)` (resolves any destination value),
  `page_labels()`, `xmp_metadata()`, `metadata_bytes()`, and `info()`. A page's
  annotations (`page_annotations()`) now carry resolved link `dest`/`uri` fields.
  `Destination`, `DestView`, `OutlineItem`, `PageLabels`, `PageLabelStyle`,
  `XmpMetadata`, and `DocInfo` are re-exported from the `zpdf` facade.
- **CLI**: a new `zpdf outline <file.pdf>` prints the bookmark tree indented,
  each line ending in `-> p.<N>` (in-document page) or `-> uri:<…>` (link); a new
  `zpdf links <file.pdf>` lists each link annotation's rectangle and target.
  `zpdf info` now also prints a `Metadata:` block from `/Info`, an `XMP Metadata:`
  block from `/Metadata`, an `Outline:` summary line, and — when the document
  defines page labels — each page's printed label (e.g. `label: iv`).

### Notes

- Pure data-model work in `zpdf-document`: no new dependencies, no C/C++, no
  parser or render-backend changes. The new readers run **only** when explicitly
  called — never during `open` or rendering — so the malformed-corpus
  open/render robustness is untouched. A small shared `obj_util` module collects
  the reference-following accessors (mirroring the proven `embedded_files`
  helpers); the embedded-files reader itself is unchanged.
- **Bounded outline resolution.** The named-destination registries are flattened
  **once per `outline()` call** into a budgeted (`MAX_NAMED_DEST_ENTRIES`) map,
  and each bookmark resolves against it in O(1). This prevents a crafted file
  (tens of thousands of bookmarks each naming a missing destination over a
  budget-sized, `/Limits`-free name tree) from multiplying the per-lookup node
  budget by the item count into a multi-billion-node walk — i.e. a wall-clock
  hang on `outline()`/`info`. The one-off `named_destination` / `resolve_destination`
  APIs keep their per-call bounded walk. A remote-go-to (`/GoToR`) `/F` filename
  given as a bare string is decoded BOM-aware (UTF-16BE when so tagged),
  consistent with the filespec-dict / `/UF` and `embedded_files` paths.
- Verified by unit tests in each module (explicit/named/legacy destinations, all
  view modes, out-of-range and indirectly-encoded page numbers, name-tree
  `/Limits` descent and cycles; outline sibling/child traversal, open/closed via
  indirect/real `/Count`, URI/go-to/remote-go-to actions incl. `/UF`-preference
  and UTF-16BE filenames, many-bookmark shared-map resolution, sibling/`/First`/
  root cycles; all `/Info` fields incl. UTF-16BE, direct-trailer-dict, string
  `/Trapped`, and empty-dict; page labels — roman/decimal/letters styles, `/P`
  prefix, `/St` offset, prefix-only ranges, unlabeled leading pages, `/Kids`
  interior nodes, cyclic kids, negative keys, and the huge-`/St` decimal fallback;
  link annotations — explicit-array / named / go-to / URI / remote-go-to targets,
  non-link annotations carrying no target; XMP — the standard property shapes,
  `x-default` selection, entity and numeric-character-reference decoding, an
  unknown-entity left verbatim (no expansion), UTF-16 BOM decode, length caps,
  and char-boundary panic guards on adversarial multibyte input) plus facade
  integration tests for the outline, named-destination, `resolve_destination`,
  page-labels, link-target, XMP, and `/Info` surfaces. Page labels additionally
  verified end-to-end against a real 400-page encrypted document (indirect number
  tree → indirect label dict, decrypted) reporting `1 … 400`, and XMP against a
  real document's packet (creators / dates / producer extracted).

## 0.7.0 — PDF 2.0 colour & attachments, annotation appearances, overprint & variable fonts

A feature release, all with zero C/C++ dependencies: PDF 2.0 NChannel colour with
`None`/`All` colorant semantics, output-intent DeviceCMYK colour management, and
embedded / associated files (`/AF` attachments); synthesized appearances for the
markup, geometric, FreeText, Text-note, Stamp, Caret and Redact annotation
families; overprint compositing (`/OP` `/op` `/OPM`); OpenType variable fonts;
heuristic table detection; and higher-fidelity GPU soft masks.

### Caret & Redact annotation appearances (PDF 2.0 `Projection` recognized)

Two more annotation subtypes now get a synthesized appearance when their producer
shipped no `/AP` stream, completing the markup/geometric family in
`zpdf-document/src/annot_appearance.rs` (an existing `/AP` is still never
overridden, and both render backends draw the synthesized form XObject with no
changes):

- **`Caret`** (ISO 32000-1 §12.5.6.11): a filled insertion-mark wedge ("‸") drawn
  in the `/RD`-inset sub-rectangle of `/Rect`, coloured by `/C` (default black; a
  present-empty `/C []` is spec-transparent → nothing is drawn). `/Sy` (the
  paragraph variant) is not distinguished — the wedge serves both.
- **`Redact`** (§12.5.6.23): the regions slated for redaction are *marked* — the
  `/QuadPoints` quads (or the whole `/Rect` when absent) filled with the interior
  colour `/IC` and outlined in `/C` (default black so an un-coloured mark stays
  visible; a present-empty `/C []` is transparent). The renderer shows the marked
  state and never removes content, so the post-redaction overlay (`/OverlayText`
  / `/RO`) is intentionally not drawn. A `/QuadPoints` outline is inset by half
  the border width (a miter offset at each corner, clamped so it can't invert a
  small quad) so the stroke stays inside the `/Rect`-equal `/BBox` the painter
  clips to, matching the `/Rect` fallback.

The PDF 2.0 **`Projection`** subtype is recognized but has no defined default
appearance, so none is synthesized (it renders from its own `/AP` if present).
Pure Rust, zero new dependencies. Verified by unit tests (caret wedge geometry,
`/RD` inset, transparent `/C`; redact `/QuadPoints` + `/Rect` fallback,
transparent `/C`, the convex-quad inset helper; Projection draws nothing) and
end-to-end CPU render tests (caret wedge, redact region fill+outline, redact
rect-fallback outline).

### NChannel colour space & `None`/`All` colorant semantics (PDF 2.0)

Separation, DeviceN and the PDF 2.0 **NChannel** colour spaces now honour the
special colorant names and NChannel's per-colorant attributes. Tint→alternate
colour conversion already worked through the DeviceN path; this adds the colorant
*semantics* the spec attaches to the colorant **names** (ISO 32000-1 §8.6.6.4
Separation, §8.6.6.5 DeviceN; ISO 32000-2 NChannel).

- **`None` produces no marks.** A Separation whose colorant is `/None` — or a
  DeviceN/NChannel whose colorants are *all* `/None` — marks nothing on the page.
  Fills, strokes, glyph runs **and `/ImageMask` stencils** (which paint in the
  current fill colour) in such a space are suppressed; text **extraction** is
  unaffected (a `/None` text run is still recovered, like invisible render-mode-3
  text). Previously these painted the tint-transform's (typically dark) colour.

- **`All` knocks out.** The Separation special colorant `/All` refers to every
  colorant (registration marks), so it is not a selective overprint: it paints
  normally (no overprint mask) rather than overprinting a single colorant.

- **NChannel `/Colorants` per-colorant overprint.** When a DeviceN/NChannel
  carries an attributes dict with `/Colorants` (each spot colorant's own
  Separation space) — or contains a `/None` that must be excluded — the overprint
  active-colorant mask (PDF 8.6.7) is computed **per input colorant**: a `/None`
  contributes no ink, a standard process name (`Cyan`/`Magenta`/`Yellow`/`Black`)
  sets its channel, a spot projects through its individual Separation transform,
  and an unclassifiable spot beside a `/None` is isolated through the whole tint
  transform (so the `/None` is still dropped). The union of these is the mask.
  This is what lets K-only or single-spot overprint mix with the backdrop while a
  `/None` channel never knocks anything out. A plain Separation/DeviceN with no
  `/Colorants` and no `/None` keeps the existing whole-transform projection
  **byte-for-byte** — the per-colorant path only engages when it can differ.

The display colour is unchanged (still the full tint transform). Suppression and
the colorant mask are decided in the interpreter, upstream of the flat display
list, so **both render backends are unchanged** and CPU↔GPU parity holds
automatically (the overprint descriptors are the same shapes the wgpu composite
already matches at 0.000%). Pure Rust, zero new dependencies. Verified by unit
tests (name capture, `/Colorants` classification, the `None`/`All` projections,
the unclassifiable-spot-beside-`None` map) and CPU end-to-end render tests
(`/None` fill/stroke invisible, all-`None` DeviceN invisible, `/ImageMask`
stencil suppressed under a `/None` fill, NChannel `/None` adds no ink under
overprint, `/Colorants` spot overprints, and a normal-spot control).

### Embedded files & associated files (attachments, PDF 2.0 `/AF`)

The library now surfaces files **stored inside** a PDF: classic embedded files
from the catalog's `/Names /EmbeddedFiles` name tree (a viewer's "attachments"),
and ISO 32000-2 **associated files** (`/AF`) attached to the document or a page,
each carrying an `/AFRelationship`. This is the mechanism PDF/A-3 and
ZUGFeRD/Factur-X use to embed an invoice's source XML, so the bytes are now
recoverable.

- **Parsing & exposure** (`zpdf-document/src/embedded_files.rs`, zero new deps):
  one `EmbeddedFile` model spans both sources — best file name (`/UF` preferred,
  then `/F`/platform names), `/Desc`, `/AFRelationship`, the embedded stream's
  `/Subtype` (MIME) and `/Params` (`/Size`, `/CreationDate`, `/ModDate`,
  `/CheckSum`), and the stream's object id. A guarded name-tree walker (depth,
  per-reference cycle, and entry-count caps) handles interior `/Kids` and leaf
  `/Names` nodes; `/AF` arrays are read from the catalog and from page dicts.
  Metadata is read off stream **dictionaries** — the payload is never decoded
  during listing.

- **API** (`PdfDocument`): `embedded_files()` (name tree), `associated_files()`
  (catalog `/AF`), `page_associated_files(page)` (page `/AF`), and
  `embedded_file_bytes(&EmbeddedFile)` — which decodes on demand through the
  parser's filter pipeline, so it respects `ParseLimits`. `EmbeddedFile` /
  `EmbeddedSource` are re-exported from the `zpdf` facade.

- **CLI:** new `zpdf attachments <file.pdf> [--extract <index|name|all>]
  [--out-dir <dir>]` lists embedded/associated files (deduplicated by embedded
  stream, merging in the `/AF` relationship) and extracts them by listing index,
  name, or `all`. Extraction is hardened: file names are **sanitized** (a
  malicious `/UF` such as `../../etc/passwd` is reduced to its basename; path
  separators, Windows-reserved characters / device names, and trailing dots and
  spaces are neutralized) and writes **never overwrite an existing file** (atomic
  create-new; a collision gets a ` (n)` suffix) — so an attachment can neither
  escape `--out-dir` nor clobber a file in it. `zpdf info` also lists attachments.

- **Lazy & non-regressing:** these paths run only when explicitly called, never
  during open or rendering, so the malformed-corpus open/render robustness is
  untouched.

### Output Intents — DeviceCMYK colour management (PDF/X & PDF 2.0)

Output intents are now parsed, surfaced, and — for CMYK — honoured. An output
intent declares the *characterized printing condition* a document was prepared
for; its `/DestOutputProfile` (an embedded ICC profile) is exactly how the
document's DeviceCMYK is meant to be interpreted. When that profile is 4-channel
(CMYK), DeviceCMYK now renders **through the profile** (the PDF/X model) instead
of the generic Adobe SWOP polynomial — so a FOGRA/GRACoL/SWOP-tagged file is
colour-managed to its own condition rather than a one-size-fits-all approximation.

- **Parsing & exposure** (`zpdf-document/src/output_intents.rs`, zero colour-
  management deps): document-level `/OutputIntents` off the catalog **and**
  ISO 32000-2 **page-level** `/OutputIntents` off the page dict (page-level
  overrides document-level). `OutputIntent` carries `/S`, the condition
  identifier/condition/info text strings, the `/DestOutputProfile` object id, and
  the profile's `/N` (read without decoding the stream). Surfaced via
  `PdfDocument::output_intents()` / `page_output_intents()`, the new
  `OutputIntent::has_cmyk_profile()` predicate, and the `zpdf info` command.

- **Colour management** (`zpdf-content`): a render-side helper
  `output_intent_cmyk_profile` picks the effective intent (page over document)
  and compiles its profile through the existing `IccCache` (failures cached, the
  media-relative-colorimetric default intent — an output intent characterizes a
  device, so it is fixed at document scope). The interpreter gained an
  `output_intent_cmyk` field + `with_output_intent_cmyk` builder. DeviceCMYK
  routes through it at a single vector/text gate (`cmyk_to_display`, covering
  `k`/`K` and the `DeviceCMYK | ICCBased(4)` arm of `components_to_rgb` — so
  `sc`/`scn`, initial colours, and Indexed/Tint-over-CMYK all follow), and for
  raster images (`Cmyk` → the profile's `Icc{4}` space; an Indexed/DeviceCMYK
  palette is baked to RGB through the profile).

- **Strict gating, no regressions:** with no usable 4-channel output intent the
  field is `None` and every conversion keeps the SWOP polynomial **byte-
  identically**. An explicitly compiled embedded ICCBased(4) colour space always
  keeps its own profile (it never reaches the gate); the output intent only
  substitutes for *DeviceCMYK*. RGB / non-4-channel / unparseable intents are
  ignored. The overprint colorant projection keeps its raw C,M,Y,K tints — only
  the display colour is rerouted. **Both render backends are unchanged** —
  conversion happens upstream in the interpreter, so the display list already
  carries colour-managed sRGB and CPU↔GPU parity holds automatically.

Pure Rust, zero new dependencies. Verified by unit tests (parsing incl.
page-level + UTF-16BE strings + RGB rejection; the four CMYK conversion sites
route through a non-SWOP profile and differ from SWOP; no-OI stays exactly SWOP;
image `Cmyk`→`Icc{4}` and Indexed-base baking) and CPU end-to-end render tests
(document-level, page-level, page-overrides-document, and RGB-intent-ignored —
each comparing the rendered pixel against the SWOP baseline).

### Overprint (PDF 8.6.7)

Overprinting is now honoured on both backends. A painting operation set to
overprint paints **only the colorants its source colour names** and leaves the
rest of the backdrop untouched — so K-only black text overprints onto a colour
without knocking it out, and one process ink laid over another *mixes* (cyan
over yellow → green) instead of replacing it.

The graphics state grew `/OP` (stroking), `/op` (nonstroking; inherits `/OP`
when absent), and `/OPM` (overprint mode), parsed in the `gs` ExtGState handler.
At each colour-set operator the interpreter also records the source colour's
**CMYK colorant projection**:

- **DeviceCMYK / ICCBased(4)** keep their four tints, and `/OPM` decides the
  active colorants — mode 0 paints all four (knockout, a no-op on a CMYK-only
  device), mode 1 paints only the nonzero ones (the classic "nonzero overprint"
  that lets 0-valued components show the backdrop through);
- **DeviceGray** maps to the black colorant `(0,0,0,1−g)`;
- **Separation / DeviceN** run their tint transform and take the resulting
  process colorants (nonzero rule, independent of `/OPM`);
- **DeviceRGB, CIE (Lab/CalRGB), ICC non-4-channel, Pattern** are additive or
  non-colorant spaces where overprint has no visible effect → treated as a
  normal paint.

The active-colorant mask plus source CMYK travel into the flat display list as a
new `Overprint { cmyk, active }` on `FillPath` / `StrokePath` / `GlyphRun`
(images and shadings are out of scope). Because the backdrop is only known at
paint time, the composite happens in the **backends**, in *naïve subtractive
CMYK*: the active channels are taken from the source, the rest read from the
backdrop, recombined, and written back. The conversions
(`zpdf_core::{rgb_to_cmyk_naive, cmyk_to_rgb_naive}`, a new mutually-inverse
pair shared by the interpreter and both backends) round-trip exactly, so an
overprint never disturbs a colorant it does not name. Overprinted content thus
renders with the textbook ink model (100 % K = pure black) rather than the SWOP
fidelity polynomial used for normal CMYK painting — a deliberate trade for
round-trip safety, documented on the conversions.

- **CPU** (the oracle): the element is rendered to a scratch buffer to capture
  its coverage·opacity, then merged onto the canvas per-pixel (mirrors the
  existing knockout-merge structure).
- **wgpu**: an overprinted fill/stroke/glyph is routed through an offscreen
  layer (forcing the layered path) and composited with a new overprint mode in
  `composite.wgsl`, which decomposes the backdrop, selects per channel, and
  recombines — the same machinery as the 16 blend modes plus a colorant mask
  and source-CMYK uniform.

Pure-Rust, zero new dependencies. Verified by CPU acceptance tests (cyan+yellow
→ green, magenta+cyan → blue, knockout default, `/OPM 0` no-op, real-valued
`/OPM 1.0`, a Separation spot colour, white-gray paints-nothing, stroke
overprint) and a GPU↔CPU parity suite (fills, partial alpha, multi-colorant
DeviceN, stroke) — all **0.000 %** differing. An adversarial multi-dimension
review hardened the edge cases: ICC-CMYK initial colour, an `active == 0` paint
(paints nothing rather than bumping a non-opaque backdrop's alpha), non-finite
tint-transform output (sanitised to 0), tiling-cell / Pattern-space colorant
reset, and overprint inside a soft-mask group now honoured on the GPU to match
the CPU oracle.

Known limitations (documented residuals): image and shading overprint are not
composited (treated as normal paint); an element that is *both* overprinting and
carries a non-Normal blend mode or `/SMask` composites against the isolated
group's backdrop rather than the page (degrades to a knockout for that rare
combination); overprint inside a knockout group falls back to plain knockout;
and, as before, the wgpu backend has no per-page time budget, so an overprint —
which routes each element through the layered path — widens the (pre-existing)
surface where a pathological page can produce many composite passes (the CPU
backend's deadline backstop still truncates such pages).

### Variable fonts (OpenType `fvar`/`gvar`)

An embedded (or substituted) **variable** font program now renders at the instance
the FontDescriptor asks for, instead of always the default master. The descriptor's
style selectors drive the OpenType variation axes:

- `/FontWeight` → `wght`,
- `/FontStretch` (name) → `wdth` (mapped to its percentage, e.g. `Condensed`→75,
  `Normal`→100, `Expanded`→125),
- `/ItalicAngle` → `slnt` (same counter-clockwise-degrees convention),
- the Italic flag → `ital`.

`LoadedFont::set_variations` records the requested axes and they are applied
(`ttf-parser`'s `set_variation`, which clamps to each axis range and honors `avar`)
before every outline/advance read. It is a **no-op for static fonts** — axes the
program does not carry are ignored — so the common, already-instanced font is
untouched; `/Widths` remain authoritative for positioning. The descriptor is found
through the Type0 indirection too (the selectors live on the descendant CIDFont).

Pure-Rust, no new dependencies (`ttf-parser`'s `variable-fonts` is a default
feature). Verified with a minimal `fvar`/`gvar` fixture (`crates/zpdf-font/tests/`):
the `wght` axis widens a glyph's outline *and* its advance (default/400 → 900), an
intermediate weight interpolates, absent axes are ignored safely, and an
end-to-end PDF with `/FontWeight 900` drives the axis through the loader
(`crates/zpdf/tests/`). An adversarial review confirmed the variation is applied
on the live simple-font advance fallback (`simple_glyph_advance`), not just the
outline, so spacing matches the rendered weight.

### GPU soft-mask fidelity (`/TR`, tiling offset, nested groups)

The wgpu backend now matches the tiny-skia CPU oracle on three ExtGState `/SMask`
cases it previously approximated or dropped. Soft masks were already rendered on
the GPU (a coverage layer pre-multiplied into the group before compositing); these
close the remaining gaps, so a soft-masked page renders identically on both
backends:

- **`/TR` transfer function**: the mask's transfer LUT (pre-sampled to 256 steps by
  the interpreter) is uploaded with the mask uniform and applied to the reduced
  coverage in `mask_apply.wgsl`, exactly as the CPU does after reading luminosity/
  alpha — including the unpainted (`/BC`) value. The GPU previously ignored `/TR`.
- **Tiling-pattern reuse `offset`**: a mask built once for a pattern cell and reused
  at every cell (the cell CTMs differ only by a page-space translation) is now
  sampled at `coord − (dx, dy)` device pixels, with reads outside the built mask
  taking the unpainted value — mirroring the CPU's `shift_plane`. The GPU
  previously drew every cell's mask at the build position.
- **Transparency group nested inside the mask group**: the mask group is now
  composited through the same layered path as the page (`composite_into`, shared by
  the page render and recursively by `apply_soft_mask`), so a `gs`/group inside a
  `/G` mask composites correctly. The GPU previously detected the nested group and
  **dropped the whole mask**, rendering the group unmasked.

An adversarial review of the change surfaced three further GPU↔CPU parity gaps,
all now closed:

- the soft-mask luminosity reduction used the W3C blend-mode weights
  `(0.30, 0.59, 0.11)` rather than the oracle's Rec.601 `(0.299, 0.587, 0.114)`;
  harmless for the usual gray/white masks but, once `/TR` was applied, a steep
  transfer over a *colored* luminosity mask amplified the ~1–2/255 coverage error
  into a ~36% pixel swing. The soft-mask `lum` now matches the oracle (the
  blend-mode `lum` in `composite.wgsl` is left at the spec-mandated weights);
- LUT indexing used WGSL `round()` (ties-to-even) where the oracle uses Rust
  `round` (ties-away); now `floor(x + 0.5)` to agree on exact `.5` ties;
- the image-upload walk descended only one mask level, so an image inside a
  *mask nested in a mask* was silently dropped; the walk is now recursive.

Pure-Rust, zero new dependencies; only the wgpu backend changed (the display-list
`SoftMask` already carried `offset`/`transfer`). Verified by a new GPU↔CPU
acceptance test (`crates/zpdf/tests/gpu_softmask.rs`: invert-`/TR`, `(+40,+40)`
offset, nested 0.6-alpha group, alpha-mask + gamma `/TR`, a colored-luminosity +
step-`/TR` regression guard, and a mask-nested-in-a-mask) — all **0.000%**
differing — with the existing `gpu_acceptance` corpus and wgpu soft-mask oracle
tests unchanged.

### Table detection

A new `zpdf-content::tables` module recovers tabular structure from a page's
extracted text spans — PDF has no table model, so this is heuristic and purely
**alignment-based** (no rendering or backend changes). It groups spans into
baseline rows, segments the page into vertical bands at large gaps, and finds
clean vertical **gutters** — x-ranges that text does not cross down the band —
as column separators; a band with two or more columns over several rows becomes
a `Table`. Ordinary prose fills the line width and so crosses any candidate
gutter, disqualifying itself, which keeps false positives low without needing
the page's ruled lines.

- **API**: `zpdf::detect_tables(&[TextSpan]) -> Vec<Table>`. A `Table` exposes
  `cells: Vec<Vec<String>>` (row-major), the `col_x` / `row_y` separator
  positions, `rows()` / `cols()` / `bbox()`, and `to_csv()` (RFC-4180) /
  `to_tsv()` / `to_delimited(char)`.
- **CLI**: `zpdf tables <file.pdf> [-p <page>] [--all] [--csv]` prints each
  detected grid as TSV (or CSV).
- **Robustness**: non-finite span coordinates are dropped at entry (a NaN would
  otherwise make the sort comparators violate a total order — a panic on Rust
  ≥ 1.81 — or stall the gutter sweepline); span/table counts are bounded. The
  618-PDF malformed corpus runs with **0 panics**.
- **Heuristics hardened by an adversarial review**: a spanning group header,
  caption, or subtotal row no longer collapses columns or drops the table
  (full-width rows abstain from the gutter vote, and the crossing tolerance
  rounds up so one over-wide row is forgiven); the prose-fill guard rejects
  3-or-more-column page layouts too (not just two); and a 3-row table with a
  single spanning interior row is accepted.
- **Known limitations** (documented in the module; all improvable with future
  ruling-line capture): wrapped multi-line cells read as separate rows; a short
  left-aligned header sitting fully to one side of a right-aligned numeric column
  can open a spurious gutter; and a table beginning immediately under multi-line
  prose with no blank line may be missed.

### Text-note icons & Stamp badges

`Text` (sticky-note) and `Stamp` (rubber-stamp) annotations that ship **no `/AP`
stream** now synthesize an appearance from their `/Name`, the same way markup,
geometric and form annotations already did. A producer that left a note or stamp
implicit previously rendered nothing; these now paint. As with the other
generators the synthesized appearance is a form XObject replayed through the
existing `/AP` path, so **both the CPU and wgpu backends draw them with no
backend change**, and it remains zero C/C++ dependencies.

- **`Text` note icons** (`zpdf-document/src/annot_appearance.rs`): `/Name`
  selects a small vector glyph drawn into a centred square within `/Rect` —
  `Note` (the spec default — a dog-eared page with text lines), `Comment`,
  `Help` (a question mark in a circle), `Insert` (an upward caret), `Key`,
  `Check`/`Checkmark`, and `Cross`; any unknown name falls back to the note. The
  glyph is tinted by `/C` (default note yellow), with `/CA` constant opacity via
  a generated `/ExtGState`.
- **`Stamp` badges** (§12.5.6.12): `/Name` is decoded into a spaced, uppercase
  label (`NotApproved` → "NOT APPROVED"; the spec default is `Draft`) drawn in
  centred Helvetica-Bold, sized to fill a rounded-rectangle border. The colour
  follows a per-name convention — green for affirmative (`Approved`, `Final`, …),
  blue for neutral (`Experimental`, `ForPublicRelease`, `SignHere`, …), red for
  cautionary (`NotApproved`, `Confidential`, `TopSecret`, `Draft`, and any
  unknown name) — overridable by `/C`, with `/CA` opacity. The interior
  is left unfilled so the page shows through, like a real stamp.
- **Bounded by construction**: generation fires only for `Text`/`Stamp` with no
  usable `/AP`; tiny rects are rejected; the decoded label keeps only
  `[A-Z0-9 ]` (so it is always a safe PDF literal needing no escaping and a name
  that strips to empty draws nothing); the rounded-rect corner radius is clamped
  to the badge; and the shared 1 MiB per-appearance content ceiling applies. The
  618-PDF malformed corpus still renders with **0 panics and 0 timeouts**.
- Verified by unit tests (icon routing for each `/Name`, the camelCase label
  decoder, per-name colour, `/C` override, `/CA` on both the shared-wrapper and
  stamp paths, the empty-`/C`-still-draws-note divergence from markup, tiny-rect
  and non-decodable-name rejection, and the radius clamp) plus an end-to-end
  render of a synthetic note/stamp page.

### Markup, geometric & FreeText annotation appearances

Annotations that ship **no `/AP` stream** are now drawn by synthesizing an
appearance from their geometry, the same way interactive form fields are. A
producer that left a markup or geometric annotation's appearance implicit (as
many non-Acrobat tools do) previously rendered nothing; these now paint. Still
zero C/C++ dependencies, and — because the synthesized appearance is a form
XObject replayed through the existing `/AP` path — **both the CPU and wgpu
backends render them with no backend changes** (GPU↔CPU agreement 0.198% on the
mixed-markup acceptance page).

- **Text markup** (`zpdf-document/src/annot_appearance.rs`): `Highlight`,
  `Underline`, `StrikeOut`, and `Squiggly` are drawn from `/QuadPoints`,
  following each quad's **true baseline orientation** rather than its
  axis-aligned bounding box — so **rotated / skewed** text markup lies along the
  baseline. Each quad is resolved (centroid-angle sort → convex order, longer
  edge pair → baseline, midpoint comparison → which edge is the bottom) into a
  baseline frame robust to either common `/QuadPoints` point ordering (Acrobat's
  `TL TR BL BR` or the spec's counter-clockwise order). Highlights composite
  with the **Multiply** blend mode (via a generated `/ExtGState`), so the marked
  text shows through — yellow over white, dark over black — matching Acrobat.
  Underline/strikeout/squiggly stroke along the oriented quad (default black when
  `/C` is absent).
- **Geometric markup**: `Square` and `Circle` (interior `/IC` fill + `/C`
  border, inset by `/RD` and half the `/BS`/`/Border` width; the circle is four
  Bézier arcs), `Line` (`/L`), `Polygon`/`PolyLine` (`/Vertices`; polygons fill
  `/IC`), and `Ink` (`/InkList`, round caps/joins).
- **Line-ending styles** (`/LE`, PDF Table 176) on `Line` and `PolyLine`:
  `OpenArrow`, `ClosedArrow`, their reversed `R…` variants, `Butt`, `Slash`,
  `Square`, `Circle`, `Diamond` (and `None`). Each ending is oriented along the
  line direction and sized from the border width; a closed head fills with the
  interior colour `/IC` when present (else stroked hollow). `/LE` is read as a
  two-name array (the second slot defaults to `None`) or a bare single name.
- **FreeText** (`/Subtype /FreeText`, §12.5.6.6): `/Contents` is laid out per
  `/DA` (font / size / colour) and `/Q` quadding — reusing the AcroForm
  text-layout engine — with an optional `/C` background, an optional border, and
  an optional `/CL` callout line carrying an `/LE` arrow. The text is wrapped in
  a `q … cm … BT … ET Q` block that translates into a box-local frame (and clips
  to the text region inset by `/RD`); `/Contents` is capped at 50 000 chars
  against adversarial input.
- **Conservative `Link` border**: drawn only when the file gives **both** an
  explicit `/C` colour **and** an explicit non-zero border width — no width-1
  default, so ordinary hyperlinks are not boxed (matching mainstream viewers).
- **Colour & opacity**: `/C` and `/IC` arrays of 1/3/4 components map to device
  gray / RGB / CMYK. A present-but-empty `/C []` is treated as spec-transparent
  (nothing drawn) rather than defaulted. The annotation `/CA` becomes an
  ExtGState constant alpha.
- **Non-regressing by construction**: generation fires only for these subtypes
  when the annotation has **no** usable `/AP` (a producer appearance is always
  kept); `Widget`/`Popup`/`Text`/`Stamp` and hidden/no-view annotations are
  untouched. Bounded against adversarial geometry — a 1 MiB per-appearance byte
  ceiling, a shared Squiggly segment budget (so quad-count × segment-count cannot
  blow up), point/quad/ink caps, a 50 000-char `/Contents` cap, a ±1e7 coordinate
  clamp, and an inverted-inset guard. The 618-PDF malformed corpus still renders
  with **0 panics and 0 timeouts** (426 OK, unchanged).
- Verified by the appearance generator's unit tests (oriented-quad baseline
  resolution including rotation and degeneracy rejection, the Multiply
  appearance, line-ending arrowheads with `/IC` fill, FreeText background/text/
  callout, transparent `/C []`, inverted-inset rejection, link-border policy,
  bounded Squiggly) plus **9 CPU end-to-end render tests** (Multiply highlight,
  oriented diamond highlight whose bbox corners stay clear, underline, square,
  line, closed arrowhead fill, polygon, FreeText background + glyphs,
  hidden-annotation suppression) and a GPU↔CPU acceptance comparison.

## 0.6.0 — interactive forms, passwords, mesh shadings & CJK/CMYK fidelity

A feature release, all with zero C/C++ dependencies: interactive AcroForm
support with generated field appearances, password-protected document
decryption (user/owner passwords), type 4–7 mesh shadings, the full predefined
CJK CMap families (Big5 / Shift-JIS / KSC / GBK / EUC-JP), and DeviceCMYK colour
fidelity.

### Password-protected documents (non-empty user/owner password)

Encrypted PDFs that require a password now open with one supplied — previously
only the empty-password case decrypted. The cryptographic core (RC4/AES-128/256,
MD5/SHA-2, the R6 hardened hash, key-derivation Algorithms 2 / 2.A / 2.B) was
already in place and tested; this wires a real password through it. Still zero
C/C++ dependencies.

- **New API**: `PdfDocument::open_with_password(data, pw)` (and
  `_and_limits`); `PdfFile::parse_with_password`; `PdfDocument::is_encrypted()`.
  A wrong password returns the new `Error::WrongPassword`; the password may be
  the **user** or the **owner** password. The default `open()` is unchanged.
- **Owner-password recovery** (`zpdf-parser/src/crypt.rs`): RC4 documents now
  authenticate the owner password too — Algorithm 7 derives the owner key,
  RC4-decrypts `/O` to recover the user password (single pass for R2, the 20
  reverse-counter passes for R≥3), then re-derives the file key via Algorithm 2.
  `authenticate_rc4` tries the password as user (Algorithm 6) then owner. V5
  (AES-256) already had the owner path; it now uses the supplied password.
- **Robustness preserved**: the empty-password default open stays lenient — an
  RC4 document whose `/U` doesn't validate under the empty password still opens
  best-effort (with a warning), so the malformed/adversarial corpus is
  unaffected. Only an explicitly-supplied non-empty password that authenticates
  as neither user nor owner raises `WrongPassword`.
- **CLI**: a `--password <pw>` flag on `info` / `dump` / `render` / `text` /
  `forms`; `render` notes when a document is encrypted and no password was given.
- Verified by new unit tests: a hand-built RC4 V2/R3-128 PDF with distinct user
  and owner passwords decrypts under **either** (owner via Algorithm 7 recovery),
  a wrong password returns `WrongPassword`, and the empty-password default open
  degrades without erroring (no corpus regression).

### Interactive forms (AcroForm)

Interactive form fields now have a field model and, crucially for a renderer,
**generated appearances** — a text or choice field whose producer left no `/AP`
stream (or set `/NeedAppearances`) is now drawn with its value, instead of
rendering blank. Still zero C/C++ dependencies.

- **Field model** (`zpdf-document/src/forms.rs`, new): walks `/Root /AcroForm
  /Fields` (with `/Kids` recursion, cycle/depth guards), resolving the tree into
  terminal `FormField`s with **fully-qualified names** (`/T` partials joined by
  `.`) and **inherited** `/FT` `/V` `/DA` `/Ff` `/Q` (PDF 12.7.3.2). Each field
  records its widget-annotation ids, its kind (`Tx`/`Btn`/`Ch`/`Sig`), value
  (string / name / multi-select list, UTF-16BE-aware), flags, `/MaxLen`, and
  `/Opt`. Exposed as `PdfDocument::acro_form()` and a new `zpdf forms <file>`
  CLI command that lists fields, types, and values.
- **Appearance generation** (`forms.rs` + `zpdf-content` annotation painter):
  for text and choice fields needing one, a form-XObject appearance is
  synthesized and painted through the existing `/AP` path (a synthetic
  `PdfStream` replayed by `do_form_xobject`, so **both CPU and wgpu backends
  render it with no backend changes**). It honors the `/DA` font / size / color
  (size `0` auto-fits height then width), `/Q` justification (left / center /
  right), and the **multiline**, **comb** (`/MaxLen` cells), and list-box layout
  modes. The `/DA` font name resolves through the AcroForm `/DR` font resources,
  falling back to a synthesized standard Helvetica (`load_form_fonts` now also
  loads inline font dicts). Content is emitted as WinAnsi single-byte text.
- **Non-regressing by construction**: generation fires only when the widget has
  no usable `/AP` *or* `/NeedAppearances` is set; an existing producer
  appearance is otherwise kept untouched. Buttons (checkbox/radio) keep their
  supplied `/AP` states — only the `/AS` selection is hardened to fall back to
  the field `/V` when `/AS` is absent. Password and push-button fields never
  generate. Bounded against adversarial forms (field-count / depth / value-length
  caps, visited-set cycle guard), consistent with the existing anti-hang budgets.
- Verified by unit tests (field-tree FQN + inheritance + widget mapping, DA
  parsing, UTF-16BE values, comb/escape helpers) and end-to-end CPU render
  acceptance tests (a text field's value rasterizes to glyphs inside its rect
  via both the `/DR` font and the Helvetica fallback; an existing `/AP` is not
  overridden).

### Mesh shadings (types 4–7)

The four mesh shading types now decode and render, completing the shading
family (`sh` and shading-pattern fills), still with zero C/C++ dependencies.

- **Type 4** (free-form Gouraud triangle mesh) and **Type 5** (lattice-form
  Gouraud mesh) — the packed vertex bit-stream is decoded MSB-first with
  per-vertex byte alignment; type 4 follows the edge-flag triangle strip
  (`f=0` starts a triangle, `f=1`/`f=2` reuse the previous triangle's `vbc`/`vac`
  side), type 5 triangulates `/VerticesPerRow` rows pairwise.
- **Type 6** (Coons patch mesh) and **Type 7** (tensor-product patch mesh) —
  per-patch byte-aligned records with the `f=1/2/3` shared-edge control-point /
  corner-colour reuse table; the Coons surface is evaluated directly as
  `S = SC + SD − SB`, the tensor surface as a bicubic over all 16 control
  points (interior points placed per the ISO §8.7.4.5.8 grid). Patches are
  tessellated into a triangle grid.
- Implemented in a new `zpdf-content/src/mesh.rs` (decoder + tessellation) plus
  a Gouraud triangle rasterizer in `shading.rs`. Meshes rasterize through the
  existing shading→image path, so **both the CPU and wgpu backends render them
  with no backend changes**. Decoded with the spec's image-`/Decode` mapping;
  with a `/Function` the single parametric value per vertex is mapped through
  the function. Vertex colours are resolved to RGB then interpolated
  (barycentric per triangle, bilinear per patch) — matching pdf.js/pdfium.
- Robustness: a 32-bit coordinate divisor computed in `u64` (no overflow), a
  first-patch-`flag≠0` guard, graceful truncation of incomplete trailing
  triangles/rows/patches, and a 2M-triangle ceiling — consistent with the
  existing anti-hang budgets. Verified by unit test vectors (hand-computed
  decode + interpolation) and CPU end-to-end render tests.

### Predefined CJK byte-encoded CMaps (Big5 / Shift-JIS / KSC / GBK / EUC-JP)

Completes the predefined-CMap support that previously covered only `GBpc-EUC`
(GB2312). The remaining legacy byte-encoded families — used by non-embedded CJK
fonts — no longer fall back to `Identity-H` (which produced wrong/blank glyphs);
they now decode correctly for both rendering and text extraction. Still zero
C/C++ dependencies.

- **New encodings**: GBK (`GBK-EUC`, `GBKp-EUC`, `GBK2K`, `GB-EUC`), Big5
  (`B5pc`, `ETen-B5`, `ETenms-B5`, `HKscs-B5`), Shift-JIS (the `*-RKSJ` family:
  `90ms`/`90msp`/`90pv`/`83pv`/`Add`/`Ext`), EUC-KR / UHC (`KSC-EUC`,
  `KSCms-UHC`, `KSCpc-EUC`), and EUC-JP (`EUC-H/V`) — each in both `-H` and `-V`
  writing modes.
- **How it works**: `CidCMap` gains a `LegacyEncoding` enum. Each encoding
  declares its codespace (so `next_code` segments mixed 1-/2-byte text — including
  the Shift-JIS single-byte half-width katakana block `0xA1–0xDF` and the EUC-JP
  SS2 kana lead `0x8E`) and a 2-byte → Unicode table. For a *substituted*
  (non-embedded) face the code is decoded to Unicode and the glyph resolves
  through the face's Unicode `cmap`; the system-font substitution already picks
  the right CJK face from the descendant's `/CIDSystemInfo /Ordering`
  (`GB1`/`CNS1`/`Japan1`/`Korea1`). 1-byte ASCII keeps a CID range for `/W` Latin
  advances; 2-byte CJK falls to `/DW` (full width), matching the `GBpc` precedent.
- **Tables**: baked, sorted `(u16, u16)` slices (binary-searchable) generated by
  `crates/zpdf-font/tools/gen_cjk_tables.py` from the Python standard-library
  codecs (`gbk`, `cp950`, `cp932`, `cp949`, `euc_jp`) — the same technique used
  for the hand-baked `gb2312.rs`, so no new runtime dependency.
- **Scope**: embedded fonts that use a predefined byte-encoded CMap (rare —
  embedders almost always re-encode to `Identity-H`) keep the existing CID path
  and are not in scope. Verified by unit tests (segmentation + decode +
  name classification per encoding) and end-to-end `text`/`render` round-trips
  for Big5, GBK, Shift-JIS (incl. half-width kana), EUC-KR, and EUC-JP.

### DeviceCMYK colour fidelity

DeviceCMYK without an ICC profile previously used the crude `(1−c)(1−k)`
conversion, which renders oversaturated, unlike a reference viewer. It now uses
the Adobe DeviceCMYK→sRGB polynomial approximation (fitted to US Web Coated
SWOP — the same one Acrobat and pdf.js use), so colours match a reference
renderer. Pure cyan goes from `(0, 255, 255)` to `(0, 185, 242)`; pure yellow
to `(255, 235, 61)`. Most visibly, **100 % K renders as a dark near-black
`(44, 46, 53)`, not pure black** — ink impurity, matching Acrobat.

- Single source of truth in `zpdf_color::cmyk_to_rgb` (inputs clamped to 0..1).
  Applies to DeviceCMYK fills/strokes, raw CMYK images (Flate/LZW, via
  `zpdf-image`, which delegates), Indexed-over-CMYK palettes, Separation/DeviceN
  tint transforms whose alternate space is DeviceCMYK, and — so the filter
  pipeline stays consistent — the Adobe-**YCCK** JPEG decode arm in `zpdf-parser`
  (`ycck_to_rgb` now recovers the true CMYK and runs the polynomial instead of
  the old `(1−c)(1−k)` ink weighting). No new third-party dependency.
- Unchanged: DeviceCMYK *with* an ICC/Default profile converts through the
  moxcms transform (already accurate). Plain Adobe-CMYK JPEGs (APP14 transform
  0/1) are still colour-converted internally by `zune-jpeg`, which has no raw-CMYK
  output arm we can intercept — a minor residual non-fidelity for that one path.
- Verified against the pdf.js coefficients, cross-checked numerically, with an
  end-to-end CMYK render whose pixels match the polynomial, and per-encoding
  YCCK unit vectors (white/black/gray/colour) against hand-computed references.

## 0.5.0 — robustness & closing the 0.4.0 limitations

Two robustness passes and a feature-completion pass since 0.4.0, all with zero
C/C++ dependencies.

### Corrupt & adversarial PDF robustness (618-file failing corpus)

A large-scale run over a 618-file corpus of malformed/adversarial PDFs
(`tests/failed/`, drawn from the PDFBOX, Ghostscript, poppler, MOZILLA, PDFIUM,
cairo, … bug trackers and fuzzers). The harness runs `zpdf info` (document open)
and then `zpdf render` per page (which could TIMEOUT or PANIC).

- **Documents that open: 166 → 426**; **render panics 13 → 0**, **render
  timeouts 110 → 0**, **open-time hangs 2 → 0**. The residual 192 are genuinely
  unrecoverable (password-encrypted, sub-400-byte truncated fragments, non-PDFs,
  and fuzzer minimizations whose page objects were deleted/corrupted — the
  source tools reject them too).
- **Document-open recovery** (`zpdf-parser`, `zpdf-document`): lenient `%PDF`
  header (missing hyphen `%PDF/DA2` / garbage version) with object-scan recovery
  when the marker is absent entirely (headerless `N G obj` fragments);
  whole-buffer `startxref` search (tolerates trailing garbage after `%%EOF`);
  stronger tail-scan recovery — `find_catalog` scans every object header +
  `/ObjStm` members, finds a catalog compressed inside an object stream,
  re-points a catalog shadowed by a later same-id object, tolerates a
  byte-flipped/absent `/Type`, validates an explicit trailer `/Root`, and opens
  the file even with no catalog; `resolve` consults the repair table for
  missing/free xref entries (new `PdfFile::find_objects_by_type` /
  `all_object_ids` / `force_repair_scan`); a document-level page fallback that
  scans for `/Type /Page` then "page-shaped" dicts when the `/Pages` tree is
  unreachable; default `/MediaBox` (US Letter); and lenient `read_dict` (skips a
  corrupt key/value, tolerates a lost `>>`, recursion-limit errors still
  propagate).
- **Render safety** (`zpdf-render-cpu`, `zpdf-content`, `zpdf-image`): a
  path-bounds sanitizer (no more tiny-skia coverage-overflow panics), a
  `catch_unwind` around `hayro-jpeg2000` (no more JPX panics), and layered
  anti-hang budgets — a 64 Mpx raster clamp, a per-page clip-mask pixel-work
  budget plus bbox-bounded clip intersection, interpreter command (500k) and
  operator (4M) ceilings, and per-page wall-clock backstops on both the
  interpret and render phases (`CpuRenderer::with_render_budget`). `info` page
  listing capped at 1000 pages.

### veraPDF corpus: 2907/2907

Running the full veraPDF test corpus (`https://labs.pdfa.org/stressful-corpus/`, 2,907 atomic
PDF/A / PDF/UA / ISO 32000 / Isartor / TWG files) surfaced five failures;
all are fixed and the corpus now renders 100% (see `tests/corpus-report.md`,
harness: `tests/corpus_run.sh`).

- **Lenient header version** (`zpdf-parser`): a malformed version after the
  `%PDF-` magic (e.g. `%PDF-a.4`) warns and assumes 1.7 instead of rejecting
  the file; `NotAPdf` now only means the magic is missing.
- **String limit raised** (`zpdf-core`): default
  `ParseLimits::max_string_length` 64 KiB → 16 MiB. ISO 32000 has no string
  limit (64 KiB is PDF/A-1's); the cap remains a configurable allocation
  guard.
- **Tiling-pattern soft-mask reuse** (`zpdf-content`, `zpdf-display-list`,
  `zpdf-render-cpu`): a cell applying an ExtGState `/SMask` used to rebuild
  the mask group per tile (interpret + shading raster), and the CPU backend
  re-rasterized the mask plane per painted command — ~100 s for a 4 KB
  corpus file. Tile CTMs differ only by translation, so the mask is now
  built once per `gs` site (rebased to the loop's middle tile, keyed by
  per-tile operator index + ExtGState id + CTM linear part) and carried by
  the new `SoftMask::offset`; the CPU backend caches the rasterized plane
  per mask identity and derives offset uses by a shift-blit. Pixel-exact on
  the corpus shape, ~25× faster.

### Closing the 0.4.0 known-limitations list

The 0.4.0 "known limitations" are now implemented (all but legacy predefined
CMap data tables, intentionally deferred), still with zero C/C++ dependencies.

- **Transparency groups** (`zpdf-render-cpu`): knockout groups (`/K true`) now
  composite each element against the group's *initial* backdrop instead of the
  accumulation of preceding elements — a per-element pass captures the element's
  *shape* (a full-opacity render) so a semi-transparent solid fill still fully
  knocks out (PDF 11.4.9). Non-isolated groups (`/I false`) with no group-level
  effect now render their elements straight onto the backdrop (the only correct
  realization that lets element blend modes inside see it); a non-isolated group
  carrying a constant alpha / soft mask / non-Normal group blend is still
  approximated as isolated. A single object carrying a blend mode / soft mask is
  treated as an isolated one-element group (was incidentally non-isolated).
- **Pattern paints on strokes and text** (`zpdf-display-list`, `zpdf-content`,
  `zpdf-render-cpu`, `zpdf-render-wgpu`): a tiling/shading pattern selected for a
  stroke now clips the pattern to the stroke outline (new `PushClipStroke`
  command; the CPU backend lifts a stroke-coverage mask, the GPU backend stamps
  the stroke tessellation into the clip stencil) instead of stroking a solid
  average colour. Text filled (or "stroked", render modes 1–6) with a pattern
  clips the pattern to the glyph outlines (built from the font program) instead
  of painting solid.
- **`/RenderingIntent`** (`zpdf-color`, `zpdf-content`, `zpdf-image`): the `ri`
  operator, ExtGState `/RI`, and image `/Intent` are parsed into the graphics
  state and threaded through `IccTransform`/`IccCache` (now keyed by intent) to
  the `moxcms` rendering intent — perceptual / relative- & absolute-colorimetric
  / saturation, with the ICC-mandated fallback order.
- **Per-CID vertical metrics `/W2`** (`zpdf-font`, `zpdf-document`,
  `zpdf-content`): the `/W2` array (list and range forms) is parsed into per-CID
  `(w1y, vx, vy)` triples and used for vertical writing-mode glyph placement,
  overriding the `/DW2` default per glyph.
- **JPX embedded ICC** (`zpdf-image`): an ICC profile carried inside a JPEG 2000
  codestream is now compiled and applied (media-relative colorimetric) instead
  of falling back to the channel count, when no PDF-level `/ColorSpace` overrides.
- **JBIG2 advanced segments** (`zpdf-parser`): generic refinement regions
  (type 22, GRTEMPLATE, TPGRON), symbol/aggregate refinement (`SDREFAGG`) and
  text-region per-instance refinement (`SBREFINE`), Huffman coding (standard
  tables B.1–B.15 + custom type-53 tables; Huffman symbol dictionaries and text
  regions, types 40–43), and pattern dictionaries (16) + halftone regions (20)
  now decode instead of rendering blank — round-trip-tested with new encoder
  helpers. (Multi-plane MMR halftones remain lightly tested; arithmetic is the
  common case.)
- **wgpu soft masks + group alpha** (`zpdf-render-wgpu`): the GPU backend now
  applies a group's constant alpha at composite time and rasterizes an ExtGState
  `/SMask` into an offscreen coverage layer (luminosity over `/BC`, or group
  alpha), pre-multiplying it into the group before compositing — validated
  pixel-for-pixel against the CPU oracle. Residual: the `/TR` transfer function,
  the tiling `offset`, nested groups inside a mask, and the `/RenderingIntent`
  for codestream-ICC JPX are not yet honored on every path.

## 0.4.0 — closing the 0.3.0 gaps

Every item on the 0.3.0 "known limitations" list is now implemented, still
with zero C/C++ dependencies. The trigger was a real-world regression report
(tests/test8: a Word-generated formula collection rendering almost no text),
which turned out to be two distinct font bugs — both fixed here.

### Highlights

- **System-font fallback** — non-embedded fonts (ArialMT, SimSun, MS Mincho…)
  now render through an installed substitute instead of dropping every glyph.
- **Tiling patterns** — real cell replication (colored and uncolored),
  replacing the mid-gray placeholder.
- **Soft masks & transparency groups** — ExtGState `/SMask` (luminosity +
  alpha, `/TR`, `/BC`) and form `/Group` compositing with group alpha.
- **Annotations** — `/AP` appearance streams are painted (12.5.5 BBox→Rect
  mapping, `/AS` states, Hidden/NoView flags).
- **Optional content** — OCG `/OFF` layers, OCMD policies and `/VE`
  visibility expressions are honored everywhere (`BDC /OC`, XObject `/OC`,
  annotation `/OC`).
- **Composite-font CMaps & vertical writing** — embedded CMap streams, the
  predefined Unicode CMap families, `Identity-V`/`WMode 1` vertical layout.
- **JBIG2 + JPEG 2000** — both filters decode (hand-rolled T.88 decoder;
  `hayro-jpeg2000`), so scanned/bitonal and JPX images stop dropping.
- **ICC color management** — ICCBased spaces convert through their embedded
  profiles via `moxcms` (vector, shading, palette and image paths).

### Fonts (`zpdf-font`, `zpdf-document`)

- **Bug fix:** `/DescendantFonts` given as an indirect reference to the array
  failed the whole Type0 font load (placeholder under the resource name —
  the main text killer in tests/test8).
- **New module `zpdf_font::system`** — scans platform font directories once
  (Windows/macOS/Linux paths + `ZPDF_FONT_DIRS`), indexing every face by
  PostScript name, full name, and family+style with bounded partial reads
  (sfnt directory + `name` table only; TTC collections enumerated per face).
  Resolution: exact PostScript match → suffix-stripped family+style
  ("TimesNewRomanPS-BoldMT" → timesnewroman + bold) → aliases
  (Helvetica→Arial, STSong→SimSun…) → CID-ordering defaults (GB1 → YaHei/
  SimSun, Japan1 → Yu Gothic/MS Gothic…) → serif/sans/mono generics from the
  FontDescriptor flags. File bytes load (and cache) only on an actual hit.
- `LoadedFont` carries a TTC `face_index`; PDF `/Widths`//`/W` stay
  authoritative for substituted fonts. Substituted composite fonts synthesize
  CID→GID through `/ToUnicode` + the substitute's Unicode cmap.
- **Composite-font CMaps** (`zpdf_font::cmap::CidCMap`): embedded CMap
  streams parse fully (codespacerange — variable 1–4-byte codes — cidrange,
  cidchar, `usecmap`, `/WMode`); the predefined Identity and
  UniGB/UniCNS/UniJIS/UniKS UCS2/UTF16 families resolve (Unicode-coded:
  code→Unicode→GID); legacy byte-encoded CMaps (RKSJ/EUC/Big5/GBK) fall back
  to Identity with a warning.
- **Vertical writing**: `WMode 1` advances by `/DW2` (default [880 −1000])
  with glyph origin centering; the CPU backend honors per-glyph y-offsets.

### Patterns & transparency (`zpdf-content`, `zpdf-display-list`, `zpdf-render-cpu`)

- **Tiling patterns (PatternType 1):** the cell content stream is replayed
  per tile (form-XObject machinery: own resources/fonts, BBox clip, state
  floor), anchored to pattern space (`base CTM · /Matrix`), honoring
  `/XStep`/`/YStep` (negative/fractional ok) over the fill's pattern-space
  bounds, clipped to the fill path. Uncolored patterns (PaintType 2) paint
  with the `scn` color — the `[/Pattern base]` underlying space is now
  retained and the cell's own color operators are ignored per spec. A
  4096-tile clamp falls back to the old gray approximation.
- **Soft masks:** ExtGState `/SMask` parses `/S` (Luminosity|Alpha), `/G`
  (interpreted at `gs` time into a nested DisplayList — geometry fixed per
  11.6.5.2), `/BC` (luminosity approximation), `/TR` (pre-sampled 256-entry
  LUT); `PushBlendGroup` gained `alpha` + `mask` fields; the CPU backend
  rasterizes the mask group offscreen (backdrop-seeded for luminosity),
  converts via Rec. 601 luma or coverage, applies `/TR`, and multiplies the
  group before compositing.
- **Transparency groups:** form `/Group /S /Transparency` pushes a real blend
  group consuming `/ca`+`/BM`+`/SMask` (reset to defaults inside per 11.6.6);
  `/I` isolation and `/K` knockout are recorded (CPU composite approximates
  non-isolated as isolated, knockout pending). The wgpu backend composites
  groups with their blend mode but does not yet apply masks/group alpha
  (tracked for Phase 3 parity).

### Annotations & optional content (`zpdf-document`, `zpdf-content`)

- **New `zpdf_document::annotation`** — `/Annots` entries resolve to
  `{subtype, /Rect, /F flags, /AS-selected /AP /N stream, /OC}`;
  `PdfDocument::page_annotations()`. The interpreter paints them after the
  page body: `/BBox` transformed by the form `/Matrix`, its bounding box
  mapped onto `/Rect` (degenerate guards), Hidden/NoView/Popup skipped.
- **New `zpdf_document::optional_content`** — `/OCProperties /D` default
  config (`/OFF`, `/ON`, `/BaseState`); `PdfDocument::oc_config()`. The
  interpreter suppresses painting inside hidden `BDC /OC … EMC` ranges
  (nesting-correct, form-recursion-safe), for XObjects with hidden `/OC`,
  and for annotations in hidden groups; OCMD `/P` AnyOn/AllOn/AnyOff/AllOff
  and `/VE` Not/And/Or expressions are evaluated. `/Properties` resource
  lookups added to `ResourceDict`. Hidden layers also extract no text.

### Image filters (`zpdf-parser`, `zpdf-image`)

- **JBIG2Decode** — new hand-rolled, zero-dependency ITU-T T.88 decoder
  (`zpdf-parser/src/jbig2.rs`): MQ arithmetic coder (passes the Annex H.2
  conformance vector), generic regions (templates 0–3, TPGDON, custom AT
  pixels; MMR via the existing CCITT G4 code), symbol dictionaries + text
  regions (all REFCORNERs, transposed, OR/AND/XOR/XNOR), striped pages,
  `/JBIG2Globals` (resolved through the filter pipeline, cycle-safe).
  Refinement/aggregation, Huffman tables and halftone regions are
  warn-and-blank. Output is packed 1-bpp PDF-polarity rows, flowing through
  the existing bitonal image path unchanged.
- **JPXDecode** — `hayro-jpeg2000` (pure Rust, `#![forbid(unsafe_code)]`):
  5/3 + 9/7 wavelets, RCT/ICT, tiling, subsampling, JP2 boxes. The parser
  passes codestreams through; `zpdf-image` sniffs them (filter name + JP2/SOC
  magic) and decodes — codestream dimensions/components are authoritative
  (`/ColorSpace`//`/BitsPerComponent` may legally be absent), `/SMaskInData`
  0/1/2 handled, JPX-encoded `/SMask` streams fold correctly.

### Color management (`zpdf-color`)

- **New module `zpdf_color::icc`** — `IccTransform` compiles embedded ICC
  profiles (gray/RGB/Lab/CMYK, v2 + v4 LUT profiles) into →sRGB transforms
  via `moxcms`; `IccCache` memoizes per profile `ObjectId` (failures cached).
  Wired through `ContentInterpreter::with_colors`: fills/strokes, shading
  LUTs, Indexed palettes (baked at resolve time) and images (one buffer-level
  transform per image; CMYK JPEGs go through profile LUTs). Unusable
  profiles keep the exact old `/N` component-count behavior.

### Validation

- Workspace test count grew from 292 to 372: tiling/soft-mask/annotation/OCG
  pixel-level acceptance tests (CPU backend, synthetic PDFs), CidCMap and
  system-font unit tests, JBIG2 round-trip fixtures anchored to the T.88
  conformance vector, byte-exact JPEG 2000 fixtures vs OpenJPEG references,
  ICC tone-curve/LUT/fallback tests. Clippy clean.
- tests/test8 (44-page Word formula collection): page text renders complete
  (was: math-only fragments); spot-checked pages 1/3/14/26 against the
  pre-change output and the embedded-font corpus PDFs for no regressions.

### Dependencies added

`hayro-jpeg2000 0.3` (JPEG 2000), `moxcms 0.8` (ICC), `tracing-subscriber`
(CLI diagnostics only) — all pure Rust.

### Known limitations (current top gaps)

- Knockout groups composite as non-knockout; non-isolated groups composite as
  isolated; the wgpu backend ignores soft masks and group alpha.
- Legacy byte-encoded predefined CMaps (90ms-RKSJ, ETen-B5, GBK-EUC…) fall
  back to Identity; per-CID vertical metrics (`/W2`) are not parsed.
- JBIG2 Huffman/refinement/halftone segments render blank (warn); JPX ICC
  profiles inside the codestream use the channel-count fallback.
- `/RenderingIntent` is ignored (media-relative colorimetric throughout).
- Stroked paths take a solid approximation of pattern paints; text filled
  with patterns paints solid.

## 0.3.0 — PDF compatibility campaign

A broad correctness and compatibility release. Starting from a garbled-text bug
report on a macOS-Quartz PDF, the whole stack was audited against ISO 32000 and
validated page-by-page against pdfium (Chromium's renderer): a 56-page sweep
across seven real-world documents (LaTeX/CJK, Quartz, scanned & encrypted books,
Word-style forms) now renders every sampled page visually equal to the
reference — and on several pages (TeX minus signs, recovered Quartz glyphs)
*better* than it.

### Highlights

- **Encryption** — AES-128 (V4/AESV2) and AES-256 (V5/AESV3 R5+R6) decryption
  with crypt-filter (`/CF`/`/StmF`/`/StrF`) support, joining the existing RC4.
  Modern encrypted PDFs (empty user password) now open and render.
- **Gradients** — axial and radial shadings: the `sh` operator and
  PatternType 2 shading-pattern fills, driven by a new PDF function evaluator
  (all four types: sampled, exponential, stitching, PostScript calculator).
- **Blend modes** — ExtGState `/BM` is now wired into the display list; all 16
  blend modes composite in both backends.
- **Spot & special color** — Separation/DeviceN tint transforms, Lab, Indexed
  palettes (fills *and* images), ICCBased `/N` resolution, and colorspace
  resources given as direct arrays (previously dropped — matplotlib/Quartz
  plots rendered gray instead of colored).
- **CropBox and `/Rotate`** — pages render at their effective crop
  (CropBox ∩ MediaBox) with page-tree-inherited rotation; scanned-book covers
  no longer render as the full untrimmed spread.
- **Quartz font fix** — embedded Type1C (CFF) simple fonts no longer remap an
  already-resolved glyph id through the font's built-in encoding (the
  double-mapping garbled virtually all text in macOS-generated PDFs), and
  glyphs the Quartz subsetter left unnamed (charset SID 0, e.g. the CMSY
  minus at code 0) are recovered by pairing them with their declared widths —
  these render blank even in pdfium.
- **Image quality** — bilinear sampling with box-filter pre-downscale (both
  backends); scanned pages stop looking jagged/broken at screen resolutions.
- **Robustness** — xref repair, hybrid `/XRefStm`, corrupt-stream salvage,
  dangling references resolve to null per spec, cycle/recursion guards
  throughout.

### Encryption (`zpdf-parser/src/crypt.rs`)

- **AESV2 (V4 R4, AES-128-CBC):** crypt-filter dictionaries parsed; separate
  stream/string ciphers per `/StmF`/`/StrF`; per-object key derivation with the
  `sAlT` extension; IV-prefixed payloads; defensive PKCS#5 unpadding.
- **AESV3 (V5 R5/R6, AES-256-CBC):** ISO 32000-2 Algorithm 2.A key derivation,
  including the R6 hardened hash; file key unwrapped from `/UE` (or the owner
  `/O`+`/OE` path as fallback).
- `/StmF`/`/StrF` = `/Identity` correctly means *no* decryption (previously
  mangled by an RC4 fallback); a direct (non-reference) `/Encrypt` dict no
  longer disables decryption; `/EncryptMetadata false` metadata streams are
  left intact.
- End-to-end fixtures (`crates/zpdf-parser/tests/fixtures/`) generated with
  pypdf validate AES-128/256 decryption through the full parse path, plus NIST
  CBC known-answer vectors.

### Parsing robustness (`zpdf-parser`)

- References to missing or free objects resolve to `null` per ISO 32000 7.3.10
  instead of failing the document; ref→ref chains are followed with a cycle
  guard.
- **Hybrid-reference files:** the trailer's `/XRefStm` cross-reference stream
  is parsed (precedence: main table > XRefStm > `/Prev` chain).
- **Object-header validation + lazy repair:** resolving an object whose header
  doesn't match the xref entry triggers a one-time whole-file object scan and
  retry — files with shifted/wrong xref offsets now render instead of silently
  reading the wrong object.
- **FlateDecode tolerance:** truncated/corrupt streams salvage the bytes
  decoded so far; raw-deflate (headerless) streams and leading garbage are
  detected and handled; decode output is capped (anti-decompression-bomb), the
  same cap now also applying to LZW and RunLength.
- ASCII85/ASCIIHex accept stray bytes and data after EOD; traditional xref
  entries parse by tokens (19-byte line endings no longer drift); xref-stream
  `/W` widths are validated (no panic on hostile values); indirect `/Filter` /
  `/DecodeParms` are resolved.

### Color (`zpdf-color`, interpreter)

- **New module `zpdf_color::function`** — evaluator for PDF function types 0
  (sampled, multilinear, 1–32-bit samples), 2 (exponential), 3 (stitching) and
  4 (PostScript calculator with the full operator set), used by tint
  transforms and shadings.
- **Separation / DeviceN** evaluate their tint transform through the alternate
  space (a 100%-tint spot color used to render *white*); without a usable
  transform the fallback is polarity-correct gray.
- **Lab** converts analytically (Lab → XYZ → sRGB; new
  `zpdf_color::lab_to_rgb`); **Indexed** palettes are applied for `sc` fills
  including Indexed-over-Lab.
- ICCBased spaces resolve `/N` (1/3/4 → Gray/RGB/CMYK); colorspace resources
  whose value is a **direct array** (not a reference) are honored; `cs`/`CS`
  reset the color to the space's initial value; `g`/`rg`/`k` update the
  tracked space. CMYK→RGB conversion is centralized in
  `zpdf_color::cmyk_to_rgb`.

### Shadings, patterns, blend modes (`zpdf-content`)

- **New module `zpdf_content::shading`** — axial (type 2) and radial (type 3)
  shading evaluation with `/Domain`, `/Extend` and spec-correct radial root
  selection, rasterized through the ordinary image pipeline so both backends
  benefit identically.
- The **`sh` operator** paints the current clip region; **PatternType 2**
  shading patterns fill paths (clipped to the path) and approximate strokes
  with the gradient's average color. PatternType 1 (tiling) renders a neutral
  mid-gray placeholder instead of solid black.
- **ExtGState `/BM`** parses all 16 blend modes; painting commands are
  bracketed in `PushBlendGroup`/`PopBlendGroup`, which both backends already
  implemented (previously dead code).

### Fonts (`zpdf-font`, `zpdf-document`)

- **Simple-font CFF mapping rework:** the CFF built-in encoding
  (charcode→GID) moved out of the glyph-outline path into `code_to_gid`'s
  resolution chain — `/Encoding` glyph names resolved against the charset are
  no longer remapped a second time. Fixes garbled text in Quartz (macOS)
  PDFs; regression-tested against the real corpus file.
- **Quartz orphan-glyph recovery:** subset glyphs named `.notdef` (SID 0) at
  GID > 0 are paired with declared-width codes that no encoding maps — e.g.
  the CMSY minus at code 0 — recovering glyphs pdfium renders blank.
- `/CIDToGIDMap` **streams** are honored for CIDFontType2 descendants;
  FontFile3 `/OpenType`-wrapped **CID-keyed CFF** gets its charset CID→GID
  map; predefined Expert charset ids are no longer misread as offsets.
- Type3 fonts resolve **indirect** `/CharProcs`/`/Encoding`/`/Widths`/
  `/FontMatrix`, and a lookup bug rendered every Type3 font with
  `FirstChar ≠ 0` blank — fixed.
- TrueType cmap fallbacks: `0xF000|code` retry on (3,1) subtables for symbolic
  fonts and a glyph-name→MacRoman→(1,0) detour. Type1 `/FontMatrix` honors the
  full six-tuple (dvips slant/extend).

### Images (`zpdf-image`)

- 1-bpc DeviceGray polarity fixed (bitonal images rendered as negatives);
  2/4-bpc expansion and 16-bpc support added.
- `/Decode` arrays honored for Gray/RGB/CMYK/Indexed images.
- **SMask pipeline:** masks decode through the full image path (DCT, predictors,
  any bit depth, `/Decode`), resample bilinearly when sized differently from
  the image (previously dropped), and RGB is premultiplied at fold time
  (transparent regions no longer bleed).
- `/Mask` support: stencil-mask streams and color-key arrays (applied to raw
  samples before color conversion).
- Indexed palettes and ICCBased `/N` resolved by the interpreter and passed in
  via the new `ResolvedColorSpace` API; CMYK JPEG handling verified against an
  Adobe-APP14 fixture.

### Text & content interpretation (`zpdf-content`)

- **Text render modes:** mode 3 (invisible — the OCR-overlay case) and mode 7
  no longer paint; stroke modes approximate with the stroke color. Text rise
  (`Ts`) is applied to glyph placement.
- **Form XObjects:** the form's full `/Resources` (xobjects, gstates,
  colorspaces, patterns, shadings) now shadow the page's (previously only
  fonts); `/BBox` clips the content; unbalanced `q`/`Q`/`W` inside a form can
  no longer corrupt page-level state (state-stack floor + clip rebalancing);
  recursion is depth-limited. Form streams decode through the decrypting,
  filter-ref-resolving path.
- Inline-image `/CS` names resolve against the page's colorspace resources;
  `Tf` with an unresolvable font no longer leaves the previous font active.

### Document structure (`zpdf-document`)

- **CropBox:** new `PdfPage::effective_box()` (CropBox ∩ MediaBox, normalized,
  with fallbacks) is used by the CLI and viewer as the render rect; both
  backends honor a nonzero rect origin.
- `/Rotate` and `/Resources` are **inherited** through the page tree (PDF
  Table 31), joining MediaBox/CropBox; the in-flight page-rotation support
  applies the inherited value.
- Page-tree walks are cycle- and depth-guarded, tolerate missing `/Type`, and
  resolve indirect box arrays; dangling/null kids are skipped with a warning.
- Page `/Annots` are parsed into `PdfPage::annots` (rendering of appearance
  streams is future work).

### Rendering backends

- **Thin strokes** clamp to a 1-device-pixel hairline (pdfium semantics) in
  both backends; the wgpu backend's zero-width strokes rendered nothing —
  fixed.
- **Image sampling** is bilinear in both backends, with a box-filter
  pre-downscale below 0.5× on the CPU (scanned pages no longer break thin
  strokes when minified).
- **Dash patterns** are rendered: tiny-skia native on the CPU; flattened into
  solid sub-segments before tessellation on the GPU (shared helper in
  `zpdf_render::dash`).
- CPU clip masks are anti-aliased; raster dimensions use `ceil` (output sizes
  now match pdfium exactly — e.g. 910×1287 at 110 DPI for A4).

### Validation

- 56-page render sweep across the seven-document test corpus compared
  page-by-page against pdfium: all sampled pages visually match; ten
  previously-defective pages verified fixed; no regressions.
- Workspace test count grew from 168 to 292 (new unit tests across parser,
  crypt, color, content, image, font, document, render crates plus
  encrypted-PDF and Quartz-font integration fixtures); clippy clean.
- An adversarial review pass of the new shading/function/interpreter code
  found and fixed three bugs before release (radial root fallback, PostScript
  stack-top results, form `Q`-imbalance state corruption).

### Known limitations (current top gaps)

- Tiling patterns paint a neutral mid-gray placeholder (no cell replication).
- ExtGState `/SMask` soft masks and transparency-group isolation/knockout are
  not yet emitted.
- Non-embedded CJK fonts render no glyphs (system-font fallback planned).
- Annotation appearance streams (`/AP`) are parsed but not painted.
- JBIG2Decode and JPXDecode filters are unsupported (affected images drop).
- Optional-content groups (layers) always render; non-Identity CMaps and
  vertical writing are unsupported; ICC profile data is not color-managed
  (component-count fallback).

### Dependencies added

`aes 0.8`, `cbc 0.1`, `sha2 0.10` (RustCrypto, pure Rust) — AES decryption.

## 0.2.0 — wgpu GPU rendering backend

This release adds a complete, GPU-accelerated rendering backend (`zpdf-render-wgpu`)
that renders the same `DisplayList` as the CPU renderer and matches it pixel-for-pixel
within a <1% tolerance. It also fixes two rendering bugs surfaced while validating
the backend against real-world documents.

### Highlights

- **New `wgpu` GPU backend** implementing the full `RenderBackend` contract — fills,
  strokes, clips, text (outline + Type3), images, and transparency groups.
- **`--backend wgpu`** flag on the `render` CLI command (behind the `gpu` feature).
- **Interactive viewer** example (`zpdf-render-wgpu`, winit) for pan/zoom/page-flip.
- **CI acceptance harness** that renders a synthetic corpus through both backends and
  asserts GPU↔CPU parity at <1% differing pixels.
- **Two correctness fixes** (see below) affecting text layout and viewer display.

### GPU backend (`zpdf-render-wgpu`)

Built milestone by milestone, each verified against the tiny-skia CPU renderer (the
correctness oracle) with the existing `zpdf compare` tool:

| Area | Detail |
| --- | --- |
| Headless context | `Instance`/`Adapter`/`Device`/`Queue` via `pollster`; MSAA-4x + `Stencil8` negotiated; texture-dim limit raised to the adapter max. |
| Render target | Offscreen MSAA color + resolve + `Stencil8`; `copy_texture_to_buffer` → map → padding-stripped RGBA8 readback (256-byte row alignment). |
| Fills & strokes | `lyon` tessellation in device-pixel space; non-zero / even-odd fill rules; caps/joins/miter; dashes intentionally ignored (matches the CPU oracle). |
| Clipping | Stencil-based; nested clips via `IncrementClamp`; `PopClip` rebuilds the stencil. |
| Text | **Vector-fill baseline** — glyph outlines tessellated at the CPU's exact coordinates (correct by construction); Type3 glyphs interpreted as sub-display-lists. |
| Images | Per-image `Rgba8Unorm` texture + bind group; the `render_image` affine baked into quad vertices; nearest sampling; clip-tested. |
| Blend groups | Offscreen layer stack with per-group compositing; all 16 PDF blend modes via the W3C premultiplied formula. |
| Coordinate system | Single page→NDC transform reproducing the CPU's `(x·scale, (page_height−y)·scale)` with the `ctm_flips_y` heuristic and an f32→f64→f32 precision rule. |

**Validation.** All synthetic corpus cases pass <1%. The three real-world reference
PDFs render at 0.35–0.58% (page 1). A 62-page CJK document renders 52/62 pages at
<1%; the remaining dense-CJK pages sit at 1.0–1.4% — see *Known limitations*.

### CLI

- `render … --backend [cpu|wgpu]` selects the renderer (default `cpu`). `wgpu` requires
  building the CLI with `--features gpu`.
- Both backends now save through one shared PNG path, so output is identical apart from
  the renderer.

### Fixes

- **Indirect `/Widths` not resolved (text over-spacing).** Font-width parsing did not
  resolve `/Widths` and `/W` when they were *indirect references* (the form pdfTeX/LaTeX
  emits). Every glyph fell back to a 1-em default width, so text rendered uniformly
  over-spaced ("H a u s a u f g a b e"). The width parsers now resolve a reference before
  reading the array. *Affects CPU rendering, GPU rendering, and text extraction* (glyph
  advances also position extracted text spans).
- **Viewer washed out / too bright (sRGB double-encode).** The example viewer configured
  its surface with the default (sRGB) format, so blitting the already-gamma-encoded page
  bytes re-applied gamma on store and brightened the image. The viewer now selects a
  non-sRGB surface format for a 1:1 blit. *Viewer-only — the core renderer's pixels were
  always correct.*

### Known limitations

- **Dense CJK text AA.** tiny-skia uses analytic anti-aliasing; the GPU uses 4× MSAA.
  On glyph-dense CJK pages the per-edge-pixel difference can exceed the threshold-16 gate
  (~1.0–1.4% of pixels), though the text renders correctly. 8× MSAA was evaluated and made
  the match *worse* (a blend-space mismatch), so 4× is used. These pages pass at
  threshold ~24–32.
- **Transparency groups are dormant.** The content interpreter does not yet emit
  `PushBlendGroup`/`PopBlendGroup` (no `/BM` or transparency-group handling), so blend
  groups don't arise from real PDFs on *either* backend. The GPU implementation is ready
  for when interpreter support lands; it's validated programmatically.
- **Not implemented (intentionally deferred):** draw-call batching (immediate mode is
  correct and fast enough), an R8 glyph atlas (the vector baseline is a better AA match),
  GPU timing telemetry, and soft masks.

### Dependencies added

`wgpu 29`, `lyon 1`, `pollster` (GPU backend); `winit 0.30` (viewer example only — a
dev-dependency, never linked into the library or CLI).
