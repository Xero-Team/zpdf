#![no_main]
//! End-to-end fuzz of the whole-file parser: `PdfFile::parse_with_limits` over
//! arbitrary bytes, then — if a file is produced — resolve every object and
//! pull every stream's decoded data. This is the deepest target: it drives
//! header detection, the xref-table / xref-stream / object-stream parsers, the
//! tail-scan recovery path, lazy object decoding, and the full filter pipeline
//! including decryption. Invariant: never panic, never hang, never
//! over-allocate — the same contract the tests/failed corpus enforces, but on
//! fuzzer-generated inputs.

use libfuzzer_sys::fuzz_target;
use zpdf_core::ParseLimits;
use zpdf_parser::PdfFile;

fuzz_target!(|data: &[u8]| {
    // Tight limits keep each iteration bounded: recovery does an O(n) tail
    // scan, and a malicious /Length or predictor could otherwise try to
    // allocate large buffers.
    let limits = ParseLimits {
        max_stream_bytes: 8 * 1024 * 1024,
        max_image_pixels: 4_000_000,
        max_page_operators: 100_000,
        max_objects: 100_000,
        ..Default::default()
    };

    let file = match PdfFile::parse_with_limits(data, limits) {
        Ok(f) => f,
        Err(_) => return,
    };

    // Touch every object the parser knows about, and decode every stream. This
    // is where filter bugs, cyclic references, and decryption edge cases
    // surface. `all_object_ids` is already bounded by `max_objects`.
    for id in file.all_object_ids() {
        if let Ok(obj) = file.resolve(id) {
            if matches!(obj, zpdf_core::PdfObject::Stream(_)) {
                let _ = file.resolve_stream_data(id);
            }
        }
    }
});
