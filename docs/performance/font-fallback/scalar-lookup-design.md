# Scalar lookup work reduction

The remaining original scalar-query control warranted investigation after
63 paired observations placed its CJK median near +6%, with uncertainty
crossing the +5% gate. Old/current disassembly had the same 131 instructions
apart from addresses and field offsets. It also showed a 0xAD8-byte (2,776-byte)
Face copy on every scalar query. This does not prove the cause of the observed
delta, but it identifies avoidable work in the queried operation.

## Implementation

`unicode_lookup.rs` reads the sfnt/TTC directory using `ttf_parser::RawFace`.
For static fonts it validates the same required `head`, `hhea` and `maxp`
tables, then parses `cmap` and, when requested, `hmtx`. Subtables are selected
in the same order as `Face::glyph_index`. Missing optional metrics retain the
existing 500-unit fallback. No parsed Face cache, additional font field,
self-referential object or unsafe lifetime extension is introduced.

Directory traversal preserves ttf-parser's leniency: unsorted records work,
the last matching record wins, and an invalid later range clears an earlier
table. Checked offset overflow skips that record. A shortcut using RawFace's
binary-search table accessor would impose a sorted-directory assumption and
would not preserve this behavior.

Variable metric queries build a Face from the tables required by cmap and
horizontal variation metrics: head, hhea, maxp, cmap, hmtx, fvar, avar, HVAR,
gvar, glyf and loca. They retain ttf-parser's coordinate normalization,
glyph_hor_advance and HVAR/gvar phantom-point computation. An already parsed
directory is reused when fvar is discovered. Unrelated color/layout/name tables
are not parsed for this query. GID-only scalar requests are independent of
variation coordinates and do not compute advances. The distinction is a const
generic. Variation-selector queries retain the format-14 Face path.

The first full-Face fallback prototype added a repeated directory probe to
default variable queries. A default-scheduler run measured +5.74% (95%
+4.96..+6.07%), so that prototype was not accepted. The narrower variable-metric
candidate must pass its own comparison before the optimization is retained.

## Evidence and acceptance

Five equivalence tests cover all mapped fixture scalars, invalid scalar values,
geometric TrueType, the real Noto subset, CFF, two TTC faces, variation axes,
unsorted records, missing optional metrics and malformed required/duplicate
records. The complete font/page tests, clippy and wasm32 build pass.

Four static diagnostics improve roughly 76–78% against the unchanged original
controls. Two added variable-font diagnostics prevent an optimization of static
fonts from hiding a regression for default or configured variable instances.
Both pass in the complete eleven-case diagnostic run. Unrelated construction
and copy diagnostics still show substantial host timing uncertainty.

Complete current-source original, CMap, provider and GPU comparisons are
required before accepting this candidate. Earlier binaries' passes and a
successful diagnostic subset are not full acceptance. All measurements,
including inconclusive and rejected candidates, remain preserved.
