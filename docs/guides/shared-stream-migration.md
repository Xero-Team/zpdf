# Migrating shared stream storage

This change is intended for the next **0.x minor release**, not a 0.19.x patch.
`PdfStream.data` changes from `Arc<[u8]>` to `PdfStreamData`, exported by both
`zpdf_core` and `zpdf`. Exhaustive struct literals and Arc-specific operations
need updating. The representation of `PdfStreamData` is private.

## Reading and constructing streams

`PdfStream::new(dict, Vec<u8>)` keeps its signature. `&stream.data`, slice indexing,
`len()`, `is_empty()`, `as_ref()` and `to_vec()` continue to work. Explicit access
is available through `stream.data.as_slice()`; it never decodes or copies bytes.
Equality and Debug only inspect the visible stream bytes.

```rust
use std::sync::Arc;
use zpdf::{PdfDict, PdfStream, PdfStreamData};

let owned: Arc<[u8]> = Arc::from(b"payload".as_slice());
let stream = PdfStream { dict: PdfDict::new(), data: owned.into() };
let shared = stream.data.clone(); // replaces Arc::clone(&stream.data)
let legacy: Arc<[u8]> = shared.into_arc();

let file: Arc<[u8]> = Arc::from(b"prefixPAYLOADsuffix".as_slice());
let view = PdfStreamData::from_shared(file, 6..13)?;
assert_eq!(view.as_slice(), b"PAYLOAD");
```

`From<Vec<u8>>`, `From<Arc<[u8]>>` and `From<&[u8]>` construct independent data.
The Arc conversion reuses the supplied byte allocation. A Vec or borrowed slice
is converted to an Arc allocation. `from_shared` checks the entire range and
returns an error for reversed/out-of-bounds ranges. Empty views release the
supplied backing storage.

`into_arc()` reuses a whole byte allocation; converting a partial view copies
only its visible bytes. Use `data.clone().into_arc()` when keeping the original
handle. Arc-specific mutation APIs such as `Arc::make_mut` no longer apply:
copy with `to_vec()`, modify the Vec, then replace `stream.data` with `vec.into()`.

## Source lifetimes

`PdfFile` parses raw streams as checked ranges into its immutable source file.
Cloning a stream shares its payload. Keeping any nonempty parsed stream after
dropping the file can therefore keep the **entire source allocation** alive.
Use `stream.data = stream.data.into_detached()` when this is undesirable;
`to_detached()` returns a detached handle while preserving the original.
Already independent payload allocations remain shared.
Whole-range views also reuse their backing: there are no unrelated bytes to
release. Detachment does not promise unique ownership or a new allocation
identity, regardless of which constructor produced the view.

`copy_object_graph`, page extraction and document append/merge detach imported
stream payloads by default, so pending imported objects do not retain their
source documents. General `try_add_object` and `overwrite_object` calls preserve
the supplied storage: detach explicitly if manually importing a stream through
those lower-level APIs. Dictionaries/references still need graph remapping.

`ObjectParser::new(&[u8], &limits)` keeps its borrowed-input API and copies stream
bytes. `ObjectParser::new_shared(&Arc<[u8]>, &limits)` opts into shared storage.

## Limits and encryption

This changes raw-byte storage, not decompression, lazy-resolution or output
semantics. Decryption writes a separate payload and never mutates ciphertext in
the source file. Incremental saves preserve the original prefix; changed streams
still pass through stream serialization and the document's encryption rules.

Cache admission continues to charge each stream's visible payload, plus view
metadata, even when bytes are shared. Views are not free entries, and a zero
cache budget still disables admission. Cache charges are conservative logical
estimates, not a measure of total process RSS or the retained source file.
