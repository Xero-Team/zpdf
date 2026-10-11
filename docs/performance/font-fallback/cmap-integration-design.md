# Authoritative CMap integration

Work package: #51. Runtime replacement is implemented; final performance
acceptance remains open. See STATUS.md for candidate-specific evidence.

## Data and namespaces

Pinned Adobe encoding CMaps provide byte-length-sensitive code-to-CID
mappings. Separately pinned `pdf2unicode/Adobe-*-UCS2` resources provide
canonical CID-to-Unicode strings for GB1, CNS1, Japan1, Korea1 and KR. Keep
full strings: some contain supplementary scalars or variation selectors, and
others represent ligatures. No host GBK/CP950 codec participates in generation.

The source ZIP retains 91 verified upstream files, including both licenses.
The offline generator resolves `usecmap` inheritance, applies child overrides,
and emits shared compact sections. One/two-byte groups use a direct prefix
directory; three/four-byte groups keep a sparse sorted prefix directory. Each
selects a deduplicated 256-entry page, stored densely or sparsely according to
occupancy. Legacy entries retain CID and Unicode value; Unicode-source pages
store only CIDs and derive Unicode from the source. Multi-scalar values use a
deduplicated UTF-8 pool. Canonical collection lookup uses a direct CID-indexed
array. Scalar lookups avoid constructing string-valued results. All reads are
bounds checked, and no runtime decompression or per-map allocation occurs.

The original uniformly sparse directory had measured legacy-query regressions
of 19.61–82.39% and was rejected. The direct-directory candidate trades 14,388
additional data bytes for fewer lookups. Its performance must be measured;
compactness and constant-time indexing alone do not prove acceptance.

Unicode-family CMaps keep their source Unicode scalar semantics while using
the authoritative collection CID for PDF widths and glyph selection.
UTF-16 scalar decoding must retain the raw source code for ToUnicode lookup;
converting a scalar back to its packed surrogate pair is needed when querying
the encoded CMap table. Identity-H/V are separate: code equals CID, never
implicitly Unicode. An Identity font may still carry a known CIDSystemInfo
collection for Unicode recovery.

## Runtime integration points

- `CidCMap::predefined` must recognize exact known names, carry a shared static
  resource reference and avoid cloning large tables at construction.
- Embedded CMaps should inherit authoritative named bases, then apply their
  own cidchar/cidrange overrides with byte lengths intact. Existing public
  ToUnicode behavior should preserve multi-scalar extraction strings.
- The interpreter should compute code-to-CID once. Embedded CIDFontType2 uses
  CIDToGIDMap, embedded CFF uses its charset, and substitutes use reliable
  Unicode to obtain their own GIDs. Widths and W2 remain keyed by the original
  CID in all three paths.
- Font loading must retain known collection identity even without ToUnicode.
  It must not convert an authoritative Unicode CMap to Identity just because
  an embedded CFF program lacks a Unicode cmap.
- `build_substitute_cid_to_gid` must support four-byte source codes and raw
  UTF-16 values. Missing entries remain explicit misses, not substitute GIDs.
- The predefined-CMap writer continues emitting Unicode source codes and
  source-code-keyed ToUnicode, but its W/W2/CIDToGIDMap keys must use actual
  collection CIDs. Existing tests which only check the encoding name cannot
  establish external interoperability.

The CFF writer now retains charset CIDs and ROS metadata. Identity source codes
use those CIDs; predefined CFF authoring rejects incompatible collections and
charsets. A CIDToGIDMap stream cannot repair a CIDFontType0 descendant; that key
is emitted only for CIDFontType2. The original geometric CFF fixture deliberately
uses CIDs 7/400/655 for GIDs 1/2/3. Tests check that namespace directly as well
as PDF round trips. Font emission also bounds aggregate table copying and
rebuilds sfnt head/table checksums.

## Validation

Verify generated entries against source overlays, including all range
boundaries, missing codes, source byte lengths, surrogate pairs, variation
sequences, vertical overrides and usecmap inheritance. Known anchors include
GBK2K code 0x81308436 -> GB1 CID 22354 -> U+00A5, and HKscs-B5 code 0x8943 ->
CNS1 CID 17718 -> U+650A. Unknown variants must remain visibly unsupported.

Portable PDF fixtures must deliberately make code, Unicode, CID and GID
different. Assert embedded and substitute glyph identities, extraction and
original W/W2 placement, then CPU/SVG/GPU rendering where affected. Add a
predefined-writer round trip using checked-in fonts instead of relying on the
absent private CJK corpus. Run the original mapping/pipeline controls and new
CMap load/lookup/legacy-document cases before accepting replacement tables.

The direct Unicode field is a measured performance design, not evidence of a
speedup. Generated data size and linked executable size must be reported
separately; compact source syntax alone is not an optimization.
