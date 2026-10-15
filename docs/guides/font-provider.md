# Application-supplied fonts

`FontProvider` lets an application supply TTF, OTF or TTC bytes without
installing a system font. Embedded PDF programs keep priority. The provider is
shared by a document's pages and nested Form/appearance resource loading.

```rust
use std::sync::Arc;
use zpdf::{FontProvider, PdfDocument};

pub fn open_with_application_font(
    pdf_bytes: Vec<u8>,
    font_bytes: Vec<u8>,
) -> zpdf::Result<PdfDocument> {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register_with_aliases(font_bytes, 0, &["MissingPdfFont"])?;
    Ok(PdfDocument::open(pdf_bytes)?.with_font_provider(provider))
}
```

The optional alias is the PDF's `BaseFont` name. Registration also indexes the
font's own PostScript, full and family names. Standard six-letter PDF subset
prefixes are removed when matching. Unicode aliases are supported.

Use `FontProvider::default()` to try registered matches before the existing
installed-font lookup. `FontProvider::memory_only()` performs no system-font
discovery and is suitable for browser and headless applications. Acquisition
belongs to the host. Pass raw font bytes directly, or register a lazy pack as
described below. Raw TTF/OTF bytes do not require decompression.

## Collections, limits and ownership

The face index is zero for an ordinary TTF/OTF file. Register individual TTC
faces with their actual indices. The provider validates the index and font
program before admitting it. Identical program bytes share storage, and
multiple TTC faces charge the font program budget only once. Re-registering
the same face returns its existing `RegisteredFontId`.

`FontProvider::new(system_fonts, FontProviderLimits { ... })` configures a
per-program size limit, total retained program bytes and face count. A face
also has a cumulative name/alias limit, including repeated registration. A
failed registration does not advance generation or consume admission capacity.
The document's existing `ParseLimits::max_font_cache_bytes` still applies to
the font programs admitted to its page caches.

An `Arc<[u8]>` input is retained without copying. The caller can drop its own
reference after registration; page caches retain the selected program for as
long as a display list needs it. `FontProvider::get` returns shared bytes and
the face index for a previously registered identity.

## Registration after a document is open

Keep an `Arc` to the provider and register the new font on it. Successful
registration, including adding an alias, advances `generation()`. Subsequent
`PdfDocument::load_page_fonts` calls invalidate earlier substitute/placeholder
choices while retaining embedded programs. Existing page caches remain valid
snapshots; rebuild the page's display list to use a newly available face.

Document loading selects a whole font by name and style.
Predefined CMap coverage is described under
[PDF mappings and downloaded fonts](#pdf-mappings-and-downloaded-fonts).

## Explicit character coverage

`FontCharacter::from_text` accepts one Unicode scalar, optionally followed by
one Unicode variation selector. `resolve_character(character, hints)` selects
a registered face containing that character, then tries relevant lazy packs.
Registered faces prefer matching bold/italic metadata; unloaded packs are
tried in registration order. This API does not scan installed fonts or fetch
anything from the network. Register the desired fallback faces/packs first.

The returned `RegisteredGlyph` includes the actual GID, shared bytes, face
index and provider registration ID. A registration ID is not a page
`FontCache` ID and must not be cast to one.

Valid empty spaces are coverage hits. A variation sequence needs explicit
cmap format-14 support; an unsupported selector never silently falls back to
the base character. Arbitrary multi-scalar strings return `None` from
`from_text`, since resolving a ligature's extraction text requires a separate
shaping policy. Coverage does not by itself supply shaping or layout.

Positive and negative coverage decisions share a cache of at most 4,096
entries. Successful registration clears decisions; a generation check prevents
a racing lookup from retaining an obsolete miss. Clearing decisions never
evicts font programs or invalidates an existing returned glyph.

## Lazy font packs

```rust
use std::sync::Arc;
use zpdf::{FontPack, FontPackFormat, FontPackLimits, FontProvider};

pub fn register_cached_pack(
    provider: &FontProvider,
    encoded: Vec<u8>,
    decoded_limit: u64,
) -> zpdf::Result<()> {
    let pack = FontPack::new(encoded, FontPackFormat::Zlib, FontPackLimits {
        max_decoded_bytes: decoded_limit,
        ..Default::default()
    })?;
    provider.register_pack(Arc::new(pack), 0, &["MissingPdfFont"])?;
    Ok(())
}
```

Aliases identify a pack before decoding. Its first matching request performs
bounded decoding and font validation; later requests share `Arc` bytes.
Concurrent requests decode once. `load_pack(id)` can prewarm a registered pack
outside rendering. Failed decoding is cached and later valid candidates can
still be selected. `Raw` shares its original program bytes. `Zlib` requires a
complete single zlib stream; gzip, WOFF/WOFF2, ZIP, truncation and trailing
bytes are rejected. The renderer never guesses compression from a filename.

The provider reserves each pack's maximum decoded size before admitting it,
including storage retained after failed face validation. Reservations plus
ordinary font storage cannot exceed `max_total_bytes`. Encoded pack storage
has a separate budget of the same size. Pack and face counts are each bounded
by `max_faces`. A pack is charged once even when registered again, under
multiple aliases or TTC face indices, or rebuilt from the same bytes and
limits. Reservations are
conservative and are retained for the provider's lifetime; supply a realistic
decoded limit rather than the default for every small pack.

These budgets bound retained program bytes, not process RSS: parsing metadata,
the decoder's bounded scratch space, and the temporary Vec-to-Arc copy also
use memory. `bytes_used()`, `encoded_bytes_used()` and `reserved_pack_bytes()`
report the distinct budgets. Coverage ranges passed to `with_coverage` are
optional hints used by `resolve_character` to skip irrelevant undecoded packs;
they do not prove actual cmap coverage. Whole-face alias selection still loads
its matching pack regardless of these character hints.

## Browser

The browser exposes `Pdf.register_font(bytes, faceIndex, alias)` for raw
programs. After registration, rebuild/redraw the page; no network operation
occurs inside synchronous Rust rendering.

## PDF mappings and downloaded fonts

A font file supplies outlines and a Unicode-to-GID cmap. A PDF Encoding CMap
supplies a different mapping: its byte codes select CIDs, which address the
PDF's W/W2 metrics and the embedded font's CID namespace. A downloaded TTF
therefore complements the PDF mappings; it cannot replace them.

The predefined mapping layer now uses pinned Adobe resources for 84 named
CMaps and five canonical collections. It distinguishes source-code byte
lengths, four-byte GBK2K, UTF-16 supplementary characters, HKSCS and vertical
overrides. Identity means code equals CID, not Unicode. Known Adobe
CIDSystemInfo collections can recover Unicode without ToUnicode; an explicit
ToUnicode mapping takes precedence. Complete canonical variation sequences
remain available through `CidCMap::decode_text`.
