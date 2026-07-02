#![no_main]
//! Fuzz the indirect-object parser: `ObjectParser::parse_indirect_at(0)` over
//! arbitrary bytes. Exercises the `<num> <gen> obj … endobj` header parse, the
//! `N G R` ref promotion, and — the interesting part — the stream body
//! extraction logic (`/Length` trust vs. `endstream` scan, EOL stripping,
//! `max_stream_bytes` guard). Invariant: never panic, never hang.

use libfuzzer_sys::fuzz_target;
use zpdf_core::ParseLimits;
use zpdf_parser::ObjectParser;

fuzz_target!(|data: &[u8]| {
    // A modest stream cap keeps each iteration cheap and exercises the
    // size-limit rejection path on large declared lengths.
    let limits = ParseLimits {
        max_stream_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };
    let parser = ObjectParser::new(data, &limits);
    let _ = parser.parse_indirect_with_id(0);
});
