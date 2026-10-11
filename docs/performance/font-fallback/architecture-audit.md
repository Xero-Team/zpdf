# Font architecture and remaining coverage work

Audit scope: font loading, encoding, interpretation, outline extraction,
substitution, writer embedding and the active font-provider branch. The
reference production state is main at `137ab2b`. Provider and pack additions
are uncommitted working-tree changes; performance acceptance remains open.

## Glyphs and mapping tables solve different problems

The project does not contain a giant baked collection of Chinese glyph
outlines. `LoadedFont` parses TTF/OTF with `ttf-parser`, handles Type1 programs,
and wraps raw CFF when needed. Standard-fourteen tables contain metrics.
Encoding/AGL/CJK tables translate codes or glyph names, not glyph shapes.

The six generated two-byte decoder tables contain 76,582 `(u16, u16)` pairs:
GB2312 7,445; GBK 21,791; Big5 13,752; Shift-JIS 9,604; EUC-JP 6,942; KSC/CP949
17,048. Their element payload is 306,328 bytes (about 299 KiB), despite about
1.38 MiB of Rust source text. This is a table-payload calculation, not a linked
binary-size measurement. Lookups use binary search over sorted pairs. A full
CJK font can be much larger; downloading one does not eliminate these legacy
encoding semantics or necessarily reduce retained memory.

| Input or table | Purpose | Replaced by downloading a TTF? |
| --- | --- | --- |
| PDF `/FontFile`, `/FontFile2`, `/FontFile3` | Original glyph program, often a subset | Normally keep the embedded program |
| PDF `/Encoding` CMap | PDF byte sequence to original CID | No |
| `/CIDToGIDMap` or CFF charset | Original CID to original glyph index | No; substitute GIDs need separate mapping |
| `/ToUnicode` | Original character code to extraction text | No; one code can map to several Unicode scalars |
| Font `cmap`, `glyf`, CFF/CFF2 tables | Unicode to GID and glyph outlines in that font | A supplied font provides these |
| `/W`, `/DW`, `/W2`, `/DW2` | Authoritative PDF advances and vertical origins | Keep the PDF metrics during substitution |
| WinAnsi, MacRoman, AGL and CJK decoder tables | Legacy encoding semantics | Keep or replace with authoritative compact equivalents |

`Identity-H` means character code equals CID, not Unicode. For example, a PDF
can encode CID 42, map it to U+4E2D through `/ToUnicode`, and use GID 141 in a
substitute face. Drawing substitute GID 42 directly can paint a different
character even though a font was successfully downloaded.

## Read and render path

`zpdf-document/src/font_loader.rs` decodes embedded font streams through the
parser's bounded stream APIs, reads mappings and PDF metrics, then constructs
`LoadedFont`. Raw CFF can be wrapped as sfnt; composite CFF uses its charset,
and TrueType CID fonts use `/CIDToGIDMap` when present. Fonts are shared from a
document cache into page caches, with stable IDs retained by display lists.

Without embedded bytes, the original code queries installed fonts by name,
style, aliases and CJK collection. `system.rs` builds its directory/name index
once, using partial reads for metadata. Selected full programs are loaded on
demand and shared with weak byte-cache entries. This is whole-face matching;
it does not search another face for every missing character. A one-time system
index also does not discover newly downloaded files automatically.

`ContentInterpreter::show_text` maps codes, computes PDF advances/placement,
emits text for extraction and builds `GlyphRun` commands. Each run refers to
one font. Renderers and SVG export resolve the glyph outlines from that font.
Replacing an entire face cannot repair mixed coverage when different
characters need different faces within the same original text operation.

## Writer embedding

`DocumentBuilder::embed_font` supports WinAnsi text, while
`embed_composite_font` supports Unicode text through an Identity composite
font. During `build`, the writer collects used glyphs, performs sparse
TrueType or CFF subsetting where supported, compresses the program into
`FontFile2` or `FontFile3`, and writes widths and `/ToUnicode`. Sparse
subsetting retains glyph IDs and removes unused outlines; unsupported subset
inputs can retain the full program. This is embedding real font programs,
not generating a hardcoded glyph-shape table.

The predefined-CMap writer path currently conflates Unicode and CID in places.
Authoritative collection mappings must update both the reader and writer;
changing only one side can make internal round trips look correct while
producing PDFs with incorrect external interpretation.

## Supplied fonts and lazy packs

The active branch adds a shared `FontProvider` for in-memory TTF/OTF/TTC,
including aliases, face indices, storage bounds and generation invalidation.
It propagates into nested Forms. A `FontPack` retains raw or explicitly zlib
encoded bytes, decodes on first selection, and shares successful results and
cached failures. Capacity is reserved before lazy decode. This directly
supports host download followed by memory registration; a raw TTF/OTF does
not require decompression.

Acquisition is asynchronous host work: select a licensed source and pinned
digest, read a verified persistent cache or download with size limits, then
register bytes and rebuild the page. Do not put network requests in a glyph
lookup or synchronous renderer. Prewarm likely packs when first-page latency
matters. Keep cold download, cold decode and warm rendering measurements
separate. Loading a full pan-CJK collection to repair one glyph can consume
substantial memory; script/locale packs and coverage hints can reduce that
cost, but actual font cmap coverage remains authoritative.

## Correctness work still required

The active #50 work first prevents three existing errors: unmapped simple
codes becoming raw GIDs, absent substitute CID mappings becoming identity,
and missing Type0 programs losing their font type and PDF metrics. Its
multi-scalar policy must preserve full extraction text and avoid drawing only
the first scalar of a ligature mapping. Valid empty glyphs, especially spaces,
must remain distinguishable from unsupported characters.

The remaining per-glyph resolver should return an actual `(FontId, GID)`.
Keep original advances and positions, split visible runs on face changes,
and accumulate clipping outlines from all selected faces. Patterned text
must clip against the union once, to preserve opacity in overlapping regions.
Cache positive and negative mappings with bounded storage and provider
generation invalidation; never evict IDs retained by an existing display list.
The writer should reuse coverage selection when embedding multilingual text.

The predefined legacy CMap implementation currently has only the ASCII CID
range. Its Unicode decoder may recover outlines for a substitute while the
embedded CID path and CID-indexed widths remain incomplete. GBK2K is currently
treated as two-byte GBK and HKSCS as CP950. Complete code/CID/Unicode mappings,
four-byte and supplementary cases, vertical mappings and `usecmap` inheritance
need authoritative resources and reference-backed tests in #51.

GPU validation must also cover the glyph's vertical offset: the current
`zpdf-render-wgpu/src/glyph.rs` transform takes the positioned glyph's `x` but
does not carry its `y`. That pre-existing limitation must be resolved before
claiming vertical fallback agrees across CPU, GPU and SVG. The GPU atlas path
also extracts outlines before checking the atlas; any optimization in #52
must be measured against this actual path and retain pixel correctness.

For current tests and measurements, see [STATUS.md](STATUS.md). Tiny synthetic
or six-codepoint real fonts do not establish full CJK coverage or performance
on the absent historical corpus. Functional success alone does not satisfy
the performance gate.
