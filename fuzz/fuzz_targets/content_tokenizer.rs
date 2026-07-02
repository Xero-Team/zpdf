#![no_main]
//! Fuzz the content-stream tokenizer: `ContentTokenizer::next_token` over
//! arbitrary bytes until it returns `None`. This is the operator/operand
//! scanner that feeds the interpreter — it also handles the awkward inline
//! image (`BI … ID … EI`) length scan. Invariant: never panic, never hang.

use libfuzzer_sys::fuzz_target;
use zpdf_content::tokenizer::ContentTokenizer;

fuzz_target!(|data: &[u8]| {
    let mut tok = ContentTokenizer::new(data);

    // The tokenizer returns None at end-of-stream. Bound the loop as a hang
    // backstop: an inline-image scan consumes many bytes per token, so allow a
    // generous multiple of the input length.
    let mut budget = data.len().saturating_mul(4) + 16;
    while tok.next_token().is_some() {
        budget = match budget.checked_sub(1) {
            Some(b) => b,
            None => break,
        };
    }
});
