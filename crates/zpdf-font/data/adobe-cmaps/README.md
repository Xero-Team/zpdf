# Pinned Adobe CMap sources

The byte-for-byte upstream resources in `sources.zip` are redistributed under
the BSD-style Adobe license in `LICENSE.txt`. Both upstream license files are
also preserved in the archive. The upstream files retain their own notices.

- `adobe-type-tools/cmap-resources` at
  `f5cf3bca7fdfeaceb77aa82847e974f2306c20b4` supplies character-code-to-CID maps.
- `adobe-type-tools/mapping-resources-pdf` at
  `2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5` supplies canonical Adobe-GB1,
  CNS1, Japan1, Korea1 and KR CID-to-Unicode mappings.
- `manifest.json` records every repository, revision, path, Git blob identity,
  decoded size and SHA-256. ZIP timestamps and entry ordering are fixed.

Acquisition is a maintenance action, never a renderer operation:

```sh
python3 tools/fetch_adobe_cmaps.py
python3 tools/generate_adobe_cmaps.py
python3 tools/generate_adobe_cmaps.py --check
```

The generator uses only the standard library and checked-in sources. It does
not rely on Python's locale/codecs for GBK, Big5, Shift-JIS or Korean mappings.
It resolves source `usecmap` inheritance, preserves character-code byte
lengths, handles supplementary/multi-scalar Unicode, and emits code/CID/Unicode
records with shared duplicate sections. Source identity mismatches and malformed
maps fail rather than falling back to approximate host encodings.

The selected inventory covers currently recognized legacy and UCS2/UTF16
families and their exact known variants. Unknown names must not be silently
treated as an unrelated known variant. Identity maps remain a separate
namespace: an identity CID is not a Unicode scalar.

Production uses `maps.bin`, `text.bin` and the generated `index.rs`. Schema 5
keeps direct prefix directories for one/two-byte source codes and sparse
directories for three/four-byte codes. Large four-byte legacy directories also
use a 257-boundary high-byte index to narrow searches. Descriptors select sparse or
dense pages; Unicode-source pages store CIDs only. Canonical strings use a
direct CID-indexed table and a deduplicated UTF-8 pool. Runtime data totals
2,414,312 mapping bytes plus 18,550 text bytes.

`test-oracle.rs` contains digests computed from normalized source ranges,
independent of the packed page layout. Rust tests verify all 1,190,674 encoding
entries and 113,496 canonical strings. Generated size alone is not evidence
of faster lookup.
