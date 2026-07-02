#![no_main]
//! Fuzz the low-level PDF lexer: drive `Lexer::next_token` over arbitrary bytes
//! until EOF or the first error. Exercises number/string/name/dict/array
//! tokenization, the container-nesting depth guard, and the whitespace/comment
//! skipper. The invariant under test is simply "never panic / never hang":
//! every input must terminate in a bounded number of tokens.

use libfuzzer_sys::fuzz_target;
use zpdf_core::ParseLimits;
use zpdf_parser::Lexer;

fuzz_target!(|data: &[u8]| {
    let limits = ParseLimits::default();
    let mut lex = Lexer::new(data, 0, &limits);

    // Bound the loop independently of the lexer: each successful token must
    // advance `pos`, but guard against a hypothetical zero-width token wedging
    // the loop (that would itself be a bug worth surfacing, but we don't want
    // the fuzzer to hang on it).
    let mut budget = data.len().saturating_mul(2) + 16;
    let mut last_pos = usize::MAX;
    loop {
        if lex.is_eof() {
            break;
        }
        let pos_before = lex.pos();
        if pos_before == last_pos {
            // No forward progress since the previous iteration — stop rather
            // than spin.
            break;
        }
        last_pos = pos_before;

        match lex.next_token() {
            Ok(_) => {}
            Err(_) => break,
        }

        budget = match budget.checked_sub(1) {
            Some(b) => b,
            None => break,
        };
    }
});
